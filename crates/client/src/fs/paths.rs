//! How the client finds entries by path and inodes by entry, asking the authority when what it
//! knows (fs/inodes.rs) may be out of date (C1, C2, C7).

use std::time::Instant;
use std::{ffi::OsStr, fs::File, sync::Arc};

use fuser::{Errno, FileAttr, INodeNo};
use herdfs_core::SpacePath;
use herdfs_core::proto::Entry;

use super::{MAX_NAME, Space, TTL};
use crate::{Error, Result};

impl Space {
    /// The inode's path, which another client may have moved: once older than TTL or taken by
    /// another entry, asked of the authority by entry id (C7). Offline, the path held.
    pub(super) async fn located(&self, ino: INodeNo) -> Result<SpacePath> {
        let held = match self.bound(ino)? {
            Some((path, confirmed)) if confirmed.elapsed() < TTL => return Ok(path),
            held => held.map(|(path, _)| path),
        };
        match self.relocate(ino).await {
            Err(err) if err.is_offline() => held.ok_or(err),
            located => Ok(located?.path),
        }
    }

    /// The entry of the inode, wherever it is now.
    pub(super) async fn relocate(&self, ino: INodeNo) -> Result<Entry> {
        let asked = Instant::now();
        let entry = match self.remote.locate(self.entry_id(ino)).await {
            Err(err) => return Err(self.gone(self.path(ino)?.as_ref(), err)),
            located => located?,
        };
        self.follow(&entry, asked).await;
        Ok(entry)
    }

    /// The path of `name` in the directory `parent`, wherever that is now.
    pub(super) async fn located_child(&self, parent: INodeNo, name: &OsStr) -> Result<SpacePath> {
        if name.len() > MAX_NAME {
            return Err(Errno::ENAMETOOLONG.into());
        }
        let (name, parent) = (name.to_str().ok_or(Errno::EINVAL)?, self.located(parent).await?);
        Ok(parent.join(name).map_err(|_| Errno::EINVAL)?)
    }

    /// The entry at `path`, when it was asked for, and its journal copy while it holds changes.
    /// The entry comes from what this client knows for TTL after that, so opens see commits (C2),
    /// else from the authority; either way the reply is one snapshot of its inode (C1).
    pub(super) async fn entry(&self, path: &SpacePath) -> Result<(Entry, Instant, Option<Arc<File>>)> {
        let asked = Instant::now();
        let found = match self.cached(path) {
            // A listing leaves entries it never followed.
            Some(cached) => {
                self.move_on(&cached.0).await;
                Ok(cached)
            }
            None => self.fetch(path, asked).await,
        };
        let (entry, fetched) = match found {
            Err(err) if err.is_offline() => self.held(path).ok_or(err)?,
            found => found?,
        };
        let (entry, local) = self.view(entry);
        Ok((entry, fetched, local))
    }

    /// One snapshot of the entry's inode: its attributes as any commit of this client's since
    /// left them, with the journal copy of the changes still pending (C1).
    pub(super) fn view(&self, entry: Entry) -> (Entry, Option<Arc<File>>) {
        let view = self.inodes.with(entry.id, |state| state.view()).flatten();
        view.unwrap_or((entry, None))
    }

    /// The entry bound to `path` while it may answer for it: within TTL, and unchanged by this
    /// client since.
    fn cached(&self, path: &SpacePath) -> Option<(Entry, Instant)> {
        let (entry, asked, current) = self.inodes.at(path)?;
        (current && asked.elapsed() < TTL).then_some((entry, asked))
    }

    async fn fetch(&self, path: &SpacePath, asked: Instant) -> Result<(Entry, Instant)> {
        let entry = self.remote.stat(path.clone()).await?;
        self.follow(&entry, asked).await;
        Ok((entry, asked))
    }

    /// Offline, the last entry seen at `path`, however old, keeps cached reads and writes
    /// working (C12).
    fn held(&self, path: &SpacePath) -> Option<(Entry, Instant)> {
        let (entry, asked, _) = self.inodes.at(path)?;
        Some((entry, asked))
    }

    /// ESTALE for an entry the authority no longer finds where it was looked for. Its path is
    /// unbound, so the lookup the kernel then makes asks the authority (C7).
    pub(super) fn gone(&self, path: Option<&SpacePath>, err: Error) -> Error {
        if !err.is_not_found() {
            return err;
        }
        if let Some(path) = path {
            self.inodes.unbind(path);
        }
        Errno::ESTALE.into()
    }

    /// ESTALE unless the authority answered for the inode's own entry: its path may name another
    /// entry by now, which the kernel then looks up (C7).
    pub(super) fn expect(&self, ino: INodeNo, entry: &Entry) -> Result<()> {
        if self.ino(entry.id) == ino {
            Ok(())
        } else {
            Err(Errno::ESTALE.into())
        }
    }

    /// Counts a reply that hands the kernel a reference to the entry's inode, and returns its
    /// attributes for that reply.
    pub(super) fn looked_up(&self, entry: &Entry, asked: Instant, local: Option<&File>) -> FileAttr {
        self.inodes.looked_up(entry, asked);
        self.attr(entry, local)
    }

    /// Records an entry this client just made, whose reply hands the kernel its inode.
    pub(super) fn added(&self, entry: &Entry) -> FileAttr {
        self.inodes.stale(&entry.path);
        let local = self.journal.changes(entry.id);
        self.looked_up(entry, Instant::now(), local.as_deref())
    }

    /// Drops the kernel's `lookups` references to `ino`.
    pub fn forget(&self, ino: INodeNo, lookups: u64) {
        self.inodes.forget(self.entry_id(ino), lookups);
    }

    /// Whether the inode is known, and its path with when that was confirmed, if it has one.
    fn bound(&self, ino: INodeNo) -> Result<Option<(SpacePath, Instant)>> {
        if ino == INodeNo::ROOT {
            return Ok(Some((SpacePath::root(), Instant::now())));
        }
        Ok(self.inodes.bound(self.entry_id(ino)).ok_or(Errno::ENOENT)?)
    }

    /// The inode's path as known, without asking: `None` once another entry took it.
    pub(super) fn path(&self, ino: INodeNo) -> Result<Option<SpacePath>> {
        Ok(self.bound(ino)?.map(|(path, _)| path))
    }
}

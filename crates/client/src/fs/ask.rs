//! What the client asks the authority about entries, and how it takes the answers into what it
//! knows (fs/inodes.rs): paths other clients may have moved (C7), entries it reuses for TTL (C2),
//! and the version open files read, which an open or attribute refresh moves on (C3, C12, C14).

use std::time::Instant;
use std::{fs::File, future::Future, sync::Arc};

use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId};

use super::{InodeNo, OpenMode, Space};
use crate::{Error, Result, reader::Head};

type Held = (Entry, Instant, bool);

impl Space {
    /// The child in `parent`, when it was asked for, and its journal copy while it holds changes.
    pub(super) async fn child(
        &self,
        parent: InodeNo,
        name: String,
    ) -> Result<(Entry, Instant, Option<Arc<File>>)> {
        let asked = Instant::now();
        let parent_id = self.entry_id(parent);
        let (held, reuse) = self.held_child(parent, &name)?;
        self.answer(
            held,
            reuse,
            asked,
            Some(parent_id),
            self.remote.lookup(parent_id, name),
        )
        .await
    }

    /// A fresh child held under a fresh parent, without asking the authority.
    pub(super) async fn fresh_child(&self, parent: InodeNo, name: &str) -> Result<Option<Entry>> {
        let (held, reuse) = self.held_child(parent, name)?;
        let Some((entry, ..)) =
            held.filter(|(_, at, current)| reuse && *current && at.elapsed() < self.metadata_ttl)
        else {
            return Ok(None);
        };
        self.move_on(&entry).await;
        Ok(Some(entry))
    }

    /// The held entry, when it was asked for, and its journal copy while it holds changes:
    /// what this client knows for TTL after that, so opens see commits (C2), else the authority's
    /// answer; either way one snapshot of its inode (C1). Offline, the last entry seen, however
    /// old, keeps cached reads and writes working (C12).
    pub(super) async fn entry(&self, ino: InodeNo) -> Result<(Entry, Instant, Option<Arc<File>>)> {
        let asked = Instant::now();
        let id = self.entry_id(ino);
        let held = self.inodes.with(id, |state| state.cached()).flatten();
        let ask = async { self.remote.locate(id).await.map_err(|err| self.gone(ino, err)) };
        self.answer(held, true, asked, None, ask).await
    }

    async fn answer(
        &self,
        held: Option<Held>,
        reuse: bool,
        asked: Instant,
        parent: Option<EntryId>,
        ask: impl Future<Output = Result<Entry>>,
    ) -> Result<(Entry, Instant, Option<Arc<File>>)> {
        // A listing leaves entries it never followed.
        if let Some((entry, at, _)) = held
            .clone()
            .filter(|(_, at, current)| reuse && *current && at.elapsed() < self.metadata_ttl)
        {
            self.move_on(&entry).await;
            let (entry, local) = self.inodes.view(entry);
            return Ok((entry, at, local));
        }
        let (entry, at) = match ask.await {
            Ok(entry) => {
                match parent {
                    Some(parent) => self.follow_child(parent, &entry, asked).await,
                    None => self.follow(&entry, asked).await,
                }
                (entry, asked)
            }
            Err(err) if err.is_offline() => held.map(|(entry, at, _)| (entry, at)).ok_or(err)?,
            Err(err) => return Err(err),
        };
        let (entry, local) = self.inodes.view(entry);
        Ok((entry, at, local))
    }

    fn held_child(&self, parent: InodeNo, name: &str) -> Result<(Option<Held>, bool)> {
        let bound = self.bound(parent)?;
        let path = bound.as_ref().and_then(|(path, _)| path.join(name).ok());
        let held = path.as_ref().and_then(|path| self.inodes.at(path));
        let reuse = bound.is_some_and(|(_, at)| at.elapsed() < self.metadata_ttl);
        Ok((held, reuse))
    }

    /// ESTALE for an entry the authority no longer finds where it was looked for. Its path is
    /// unbound, so the lookup the kernel then makes asks the authority (C7).
    pub(super) fn gone(&self, ino: InodeNo, err: Error) -> Error {
        if !err.is_not_found() {
            return err;
        }
        if let Ok(Some(path)) = self.path(ino) {
            self.inodes.unbind(&path);
        }
        libc::ESTALE.into()
    }

    /// Takes an entry the authority returned, and switches an inode that open files read to a
    /// newer head it names, so every handle reads it from its next read (C3).
    pub(super) async fn follow(&self, entry: &Entry, asked: Instant) {
        self.inodes.observe(entry, asked, false);
        self.move_on(entry).await;
    }

    /// Takes a child answer and the parent id used to ask for it (C7).
    pub(super) async fn follow_child(&self, parent: EntryId, entry: &Entry, asked: Instant) {
        self.inodes.observe_child(parent, entry, asked);
        self.move_on(entry).await;
    }

    /// Switches the inode to the newer head `entry` names, fetched or cached, if its open files
    /// read an older one (C3).
    pub(super) async fn move_on(&self, entry: &Entry) {
        if self.inodes.with(entry.id, |state| state.behind()) == Some(true) {
            let _ = self.head(entry.id, OpenMode::Read).await;
        }
    }

    /// The version the entry's handles read: the one this client follows, else its head now.
    pub(super) async fn current(&self, entry: EntryId) -> Result<Head> {
        if let Some((file, _)) = self.inodes.with(entry, |state| state.known()).flatten() {
            return Ok(file);
        }
        self.head(entry, OpenMode::Read).await
    }

    /// The entry's current head, which its open files then follow. Every open asks, for
    /// close-to-open consistency (C2), and one for writing needs write access (C1, C4a).
    pub(super) async fn head(&self, entry: EntryId, mode: OpenMode) -> Result<Head> {
        let asked = Instant::now();
        let (found, head, writable) = match self.remote.open(entry).await {
            Err(err) if err.is_offline() => {
                let held = self.inodes.with(entry, |state| state.offline(mode));
                let head = held.flatten().ok_or(err)??;
                return Ok(head);
            }
            opened => opened.map_err(|err| self.gone(self.ino(entry), err))?,
        };
        self.inodes.observe(&found, asked, false);
        let switched = self.switch_version(entry, |state| Some(state.switch((head, writable))));
        if switched == Some(true) {
            self.kernel.invalidate_attrs(entry);
        }
        if mode.writes() && !writable {
            return Err(libc::EACCES.into());
        }
        let head = self.inodes.with(entry, |state| state.followed()).flatten();
        Ok(head)
    }

    /// Runs `with` on the version the entry's open files read, and once more on its head if the
    /// authority no longer lets this client use that version: it left retention, was purged, or
    /// predates the client's access (C14, S8). The file moves on as a stat would move it (C3).
    /// A second refusal is final.
    pub(super) async fn usable<T, F, Fut>(&self, entry: EntryId, with: F) -> Result<T>
    where
        F: Fn(Head) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        match with(self.current(entry).await?).await {
            Err(err) if err.is_not_found() => {
                let final_error = |err: Error| {
                    if err.errno() == libc::ESTALE {
                        libc::EIO.into()
                    } else {
                        err
                    }
                };
                let head = self.head(entry, OpenMode::Read).await.map_err(final_error)?;
                let used = with(head).await;
                used.map_err(|err| if err.is_not_found() { libc::EIO.into() } else { err })
            }
            used => used,
        }
    }

    /// Whether the inode is known, and its path with when that was confirmed, if it has one.
    fn bound(&self, ino: InodeNo) -> Result<Option<(SpacePath, Instant)>> {
        if ino == InodeNo::ROOT {
            return Ok(Some((SpacePath::root(), Instant::now())));
        }
        Ok(self.inodes.bound(self.entry_id(ino)).ok_or(libc::ENOENT)?)
    }

    /// The inode's path as known, without asking: `None` once another entry took it.
    pub(super) fn path(&self, ino: InodeNo) -> Result<Option<SpacePath>> {
        Ok(self.bound(ino)?.map(|(path, _)| path))
    }
}

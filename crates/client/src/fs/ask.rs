//! What the client asks the authority about entries, and how it takes the answers into what it
//! knows (fs/inodes.rs): paths other clients may have moved (C7), entries it reuses for TTL (C2),
//! and the version open files read, which an open or a stat moves on (C3, C12, C14).

use std::time::Instant;
use std::{ffi::OsStr, fs::File, future::Future, sync::Arc};

use fuser::{Errno, INodeNo};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId};

use super::{MAX_NAME, OpenMode, Space, TTL};
use crate::{Error, Result, reader::Head};

const MOVED_PATH_RETRIES: usize = 2;

#[derive(Clone, Copy)]
enum Follow {
    Head,
    PathOnly,
}

impl Space {
    /// The inode's path, which another client may have moved: once older than TTL or taken by
    /// another entry, asked of the authority by entry id (C7). Offline, the path held.
    pub(super) async fn located(&self, ino: INodeNo) -> Result<SpacePath> {
        self.located_for(ino, Follow::Head).await
    }

    async fn located_for(&self, ino: INodeNo, follow: Follow) -> Result<SpacePath> {
        let held = match self.bound(ino)? {
            Some((path, confirmed)) if confirmed.elapsed() < TTL => return Ok(path),
            held => held.map(|(path, _)| path),
        };
        match self.relocate_for(ino, follow).await {
            Err(err) if err.is_offline() => held.ok_or(err),
            located => Ok(located?.path),
        }
    }

    /// The entry of the inode, wherever it is now.
    pub(super) async fn relocate(&self, ino: INodeNo) -> Result<Entry> {
        self.relocate_for(ino, Follow::Head).await
    }

    async fn relocate_for(&self, ino: INodeNo, follow: Follow) -> Result<Entry> {
        let asked = Instant::now();
        let entry = match self.remote.locate(self.entry_id(ino)).await {
            Err(err) => return Err(self.gone(self.path(ino)?.as_ref(), err)),
            located => located?,
        };
        self.inodes.observe(&entry, asked, false);
        if matches!(follow, Follow::Head) {
            self.move_on(&entry).await;
        }
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

    /// Runs a namespace call below held directories, following parents renamed during the call.
    pub(super) async fn namespace<T, F, Fut>(
        &self,
        children: &[(INodeNo, &OsStr)],
        mut call: F,
    ) -> Result<(Vec<SpacePath>, T)>
    where
        F: FnMut(Vec<SpacePath>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut paths = Vec::with_capacity(children.len());
        for &(parent, name) in children {
            paths.push(self.located_child(parent, name).await?);
        }
        let mut retries = 0;
        loop {
            match call(paths.clone()).await {
                Err(err) if err.is_not_found() && retries < MOVED_PATH_RETRIES => {
                    retries += 1;
                    let mut moved = Vec::with_capacity(children.len());
                    for &(parent, name) in children {
                        let parent = self.relocate_for(parent, Follow::PathOnly).await?.path;
                        moved.push(
                            parent
                                .join(name.to_str().ok_or(Errno::EINVAL)?)
                                .map_err(|_| Errno::EINVAL)?,
                        );
                    }
                    if moved == paths {
                        return Err(err);
                    }
                    paths = moved;
                }
                answer => return answer.map(|answer| (paths, answer)),
            }
        }
    }

    /// The entry at `path`, when it was asked for, and its journal copy while it holds changes:
    /// what this client knows for TTL after that, so opens see commits (C2), else the authority's
    /// answer; either way one snapshot of its inode (C1). Offline, the last entry seen, however
    /// old, keeps cached reads and writes working (C12).
    pub(super) async fn entry(&self, path: &SpacePath) -> Result<(Entry, Instant, Option<Arc<File>>)> {
        let (asked, held) = (Instant::now(), self.inodes.at(path));
        let found = match held
            .clone()
            .filter(|(_, at, current)| *current && at.elapsed() < TTL)
        {
            // A listing leaves entries it never followed.
            Some((entry, at, _)) => {
                self.move_on(&entry).await;
                Ok((entry, at))
            }
            None => self.fetch(path, asked).await,
        };
        let (entry, at) = match found {
            Err(err) if err.is_offline() => held.map(|(entry, at, _)| (entry, at)).ok_or(err)?,
            found => found?,
        };
        let (entry, local) = self.inodes.view(entry);
        Ok((entry, at, local))
    }

    async fn fetch(&self, path: &SpacePath, asked: Instant) -> Result<(Entry, Instant)> {
        let entry = self.remote.stat(path.clone()).await?;
        self.follow(&entry, asked).await;
        Ok((entry, asked))
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

    /// Takes an entry the authority returned, and switches an inode that open files read to a
    /// newer head it names, so every handle reads it from its next read (C3).
    pub(super) async fn follow(&self, entry: &Entry, asked: Instant) {
        self.inodes.observe(entry, asked, false);
        self.move_on(entry).await;
    }

    /// Switches the inode to the newer head `entry` names, fetched or cached, if its open files
    /// read an older one (C3).
    pub(super) async fn move_on(&self, entry: &Entry) {
        if self.inodes.with(entry.id, |state| state.behind()) == Some(true) {
            let _ = self.head(entry.id, &entry.path, OpenMode::Read).await;
        }
    }

    /// The version the entry's handles read: the one this client follows, else its head now.
    pub(super) async fn current(&self, entry: EntryId) -> Result<Head> {
        if let Some((file, _)) = self.inodes.with(entry, |state| state.known()).flatten() {
            return Ok(file);
        }
        let path = self.located_for(self.ino(entry), Follow::PathOnly).await?;
        self.head(entry, &path, OpenMode::Read).await
    }

    /// The entry's current head, which its open files then follow. Every open asks, for
    /// close-to-open consistency (C2), and one for writing needs write access (C1, C4a).
    pub(super) async fn head(&self, entry: EntryId, path: &SpacePath, mode: OpenMode) -> Result<Head> {
        let asked = Instant::now();
        let (found, head, writable) = match self.remote.open(path.clone()).await {
            Err(err) if err.is_offline() => {
                let held = self.inodes.with(entry, |state| state.offline(mode));
                return Ok(held.flatten().ok_or(err)??);
            }
            opened => opened.map_err(|err| self.gone(Some(path), err))?,
        };
        self.inodes.observe(&found, asked, false);
        self.expect(self.ino(entry), &found)?;
        let switched = self.switch_version(entry, |state| {
            let switched = state.switch((head, writable));
            Some((switched, switched))
        });
        if switched == Some(true) {
            self.kernel.invalidate_attrs(entry);
        }
        if mode.writes() && !writable {
            return Err(Errno::EACCES.into());
        }
        Ok(self.inodes.with(entry, |state| state.followed()).flatten())
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
                    if err.errno() == Errno::ESTALE {
                        Errno::EIO.into()
                    } else {
                        err
                    }
                };
                let path = self
                    .located_for(self.ino(entry), Follow::PathOnly)
                    .await
                    .map_err(final_error)?;
                let head = self
                    .head(entry, &path, OpenMode::Read)
                    .await
                    .map_err(final_error)?;
                let used = with(head).await;
                used.map_err(|err| {
                    if err.is_not_found() {
                        Errno::EIO.into()
                    } else {
                        err
                    }
                })
            }
            used => used,
        }
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

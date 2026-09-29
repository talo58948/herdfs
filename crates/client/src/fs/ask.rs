//! What the client asks the authority about entries, and how it takes the answers into what it
//! knows (fs/inodes.rs): paths other clients may have moved (C7), entries it reuses for TTL (C2),
//! and the version open files read, which an open or attribute refresh moves on (C3, C12, C14).

use std::time::Instant;
use std::{fs::File, future::Future, sync::Arc};

use fuser::{Errno, INodeNo};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId};

use super::{OpenMode, Space, TTL};
use crate::{Error, Result, reader::Head};

impl Space {
    /// The child in `parent`, when it was asked for, and its journal copy while it holds changes.
    pub(super) async fn child(
        &self,
        parent: INodeNo,
        name: String,
    ) -> Result<(Entry, Instant, Option<Arc<File>>)> {
        let asked = Instant::now();
        let parent_id = self.entry_id(parent);
        let bound = self.bound(parent)?;
        let path = bound.as_ref().and_then(|(path, _)| path.join(&name).ok());
        let held = path.as_ref().and_then(|path| self.inodes.at(path));
        let fresh_parent = bound.is_some_and(|(_, at)| at.elapsed() < TTL);
        let found = match held
            .clone()
            .filter(|(_, at, current)| fresh_parent && *current && at.elapsed() < TTL)
        {
            Some((entry, at, _)) => {
                self.move_on(&entry).await;
                Ok((entry, at, false))
            }
            None => self
                .remote
                .lookup(parent_id, name)
                .await
                .map(|entry| (entry, asked, true)),
        };
        let (entry, at, fetched) = match found {
            Err(err) if err.is_offline() => held.map(|(entry, at, _)| (entry, at, false)).ok_or(err)?,
            found => found?,
        };
        if fetched {
            self.follow(&entry, asked).await;
        }
        let (entry, local) = self.inodes.view(entry);
        Ok((entry, at, local))
    }

    /// The held entry, when it was asked for, and its journal copy while it holds changes:
    /// what this client knows for TTL after that, so opens see commits (C2), else the authority's
    /// answer; either way one snapshot of its inode (C1). Offline, the last entry seen, however
    /// old, keeps cached reads and writes working (C12).
    pub(super) async fn entry(&self, ino: INodeNo) -> Result<(Entry, Instant, Option<Arc<File>>)> {
        let asked = Instant::now();
        let id = self.entry_id(ino);
        let held = self.inodes.with(id, |state| state.cached()).flatten();
        let found = match held
            .clone()
            .filter(|(_, at, current)| *current && at.elapsed() < TTL)
        {
            // A listing leaves entries it never followed.
            Some((entry, at, _)) => {
                self.move_on(&entry).await;
                Ok((entry, at))
            }
            None => self.fetch(ino, asked).await,
        };
        let (entry, at) = match found {
            Err(err) if err.is_offline() => held.map(|(entry, at, _)| (entry, at)).ok_or(err)?,
            found => found?,
        };
        let (entry, local) = self.inodes.view(entry);
        Ok((entry, at, local))
    }

    async fn fetch(&self, ino: INodeNo, asked: Instant) -> Result<(Entry, Instant)> {
        let entry = self
            .remote
            .locate(self.entry_id(ino))
            .await
            .map_err(|err| self.gone(self.path(ino).ok().flatten().as_ref(), err))?;
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
            let _ = self.head(entry.id, OpenMode::Read).await;
        }
    }

    /// The version the entry's handles read: the one this client follows, else its head now.
    pub(super) async fn current(&self, entry: EntryId) -> Result<Head> {
        if let Some((file, _)) = self.inodes.with(entry, |state| state.known()).flatten() {
            return Ok(file);
        }
        self.head(entry, OpenMode::Read).await.map(|(head, _)| head)
    }

    /// The entry's current head, which its open files then follow. Every open asks, for
    /// close-to-open consistency (C2), and one for writing needs write access (C1, C4a).
    pub(super) async fn head(&self, entry: EntryId, mode: OpenMode) -> Result<(Head, SpacePath)> {
        let asked = Instant::now();
        let (found, head, writable) = match self.remote.open(entry).await {
            Err(err) if err.is_offline() => {
                let held = self.inodes.with(entry, |state| state.offline(mode));
                let head = held.flatten().ok_or(err)??;
                let path = self.path(self.ino(entry))?.ok_or(Errno::ENOENT)?;
                return Ok((head, path));
            }
            opened => {
                let path = self.path(self.ino(entry))?;
                opened.map_err(|err| self.gone(path.as_ref(), err))?
            }
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
        let head = self.inodes.with(entry, |state| state.followed()).flatten();
        Ok((head, found.path))
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
                let head = self.head(entry, OpenMode::Read).await.map_err(final_error)?.0;
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

//! Which version an entry's open files read, and how it moves on: to the head an open or a stat
//! finds (C2, C3), to what is known offline (C12), and past a version this client may no longer
//! use (C14, S8).

use std::time::Instant;

use fuser::Errno;
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId};

use super::Space;
use crate::{Result, reader::Head};

impl Space {
    /// Runs `with` on the version the entry's open files read, and once more on its head if the
    /// authority no longer lets this client use that version: it left retention, was purged, or
    /// predates the client's access (C14, S8). The file moves on as a stat would move it (C3),
    /// and the kernel drops its old pages once this read or write, which holds some, is answered.
    /// A second refusal is final.
    pub(super) async fn usable<T, F, Fut>(&self, entry: EntryId, with: F) -> Result<T>
    where
        F: Fn(Head) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        match with(self.current(entry).await?).await {
            Err(err) if err.is_not_found() => {
                // Forgotten first, so that following the head does not wait for those pages.
                self.inodes.unfollow(entry);
                let path = self.located(self.ino(entry)).await?;
                let head = self.head(entry, &path, false, false).await?;
                self.kernel.apply(entry, false).await;
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

    /// The version the entry's handles read: the one this client follows, else its head now.
    pub(super) async fn current(&self, entry: EntryId) -> Result<Head> {
        if let Some((file, _)) = self.inodes.known(entry) {
            return Ok(file);
        }
        let path = self.located(self.ino(entry)).await?;
        self.head(entry, &path, false, false).await
    }

    /// Remembers an entry the authority returned, and switches an inode that open files read to
    /// a newer head it names, so every handle reads it from its next read (C3).
    pub(super) async fn follow(&self, entry: &Entry, asked: Instant) {
        self.inodes.observe(entry, asked, false);
        self.move_on(entry).await;
    }

    /// Switches an inode that open files read to a newer head `entry` names, fetched or cached,
    /// so every handle reads it from its next read (C3).
    pub(super) async fn move_on(&self, entry: &Entry) {
        let (open, behind) = (self.journal.is_open(&entry.id), self.inodes.behind(entry.id));
        if open && behind && self.journal.changes(entry.id).is_none() {
            let _ = self.head(entry.id, &entry.path, false, false).await;
        }
    }

    /// The entry's current head, which its open files then follow. Every open asks, for
    /// close-to-open consistency (C2), and one for writing needs write access. Offline, the
    /// head followed, with the access it came with, lets writes and cached reads go on; a
    /// truncating write needs none, since its unguarded commit is right on an unknown base too,
    /// nor does one over pending changes, which hold the bytes (C5, C12).
    pub(super) async fn head(
        &self,
        entry: EntryId,
        path: &SpacePath,
        write: bool,
        truncate: bool,
    ) -> Result<Head> {
        let asked = Instant::now();
        let (found, head, writable) = match self.remote.open(path.clone()).await {
            Err(err) if err.is_offline() => {
                return match self.inodes.known(entry) {
                    Some((_, false)) if write => Err(Errno::EACCES.into()),
                    Some((file, _)) => Ok(file),
                    None if truncate || self.journal.changes(entry).is_some() => Ok(None),
                    None => Err(err),
                };
            }
            opened => opened.map_err(|err| self.gone(Some(path), err))?,
        };
        self.inodes.observe(&found, asked, false);
        // The name now holds another entry, which the kernel looks up again (C7).
        if found.id != entry {
            return Err(Errno::ESTALE.into());
        }
        if self.inodes.switch(entry, (head, writable)) {
            self.kernel.apply(entry, true).await;
        }
        if write && !writable {
            return Err(Errno::EACCES.into());
        }
        Ok(self.inodes.followed(entry))
    }
}

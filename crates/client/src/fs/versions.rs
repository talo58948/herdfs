//! Which version an entry's open files read, and how it moves on: to the head an open or a stat
//! finds (C2, C3), to what is known offline (C12), and past a version this client may no longer
//! use (C14, S8).

use fuser::Errno;
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId};

use super::{Asked, Space};
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
                self.reader.forget(entry);
                let path = self.located(self.ino(entry)).await?;
                let head = self.head(entry, &path, false, false).await?;
                let _ = self.invalidate(entry);
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
        if let Some(file) = self.reader.current(entry) {
            return Ok(file);
        }
        let path = self.located(self.ino(entry)).await?;
        self.head(entry, &path, false, false).await
    }

    /// Remembers an entry the authority returned, and switches an inode that open files read to
    /// a newer head it names, so every handle reads it from its next read (C3).
    pub(super) async fn follow(&self, entry: &Entry, asked: Asked) {
        self.remember(entry, asked);
        let current = self.reader.current(entry.id);
        let current = current.map(|file| file.map(|file| file.version.id));
        let (open, changed) = (self.journal.is_open(&entry.id), current != Some(entry.head));
        if open && changed && self.journal.changes(entry.id).is_none() {
            let _ = self.head(entry.id, &entry.path, false, false).await;
        }
    }

    /// The entry's current head, which its open files then follow. Every open asks, for
    /// close-to-open consistency (C2), and one for writing needs write access. Offline, the
    /// head followed, with the access it came with, lets writes and cached reads go on, and a
    /// truncating write needs none, since its unguarded commit is right on an unknown base too
    /// (C5, C12).
    pub(super) async fn head(
        &self,
        entry: EntryId,
        path: &SpacePath,
        write: bool,
        truncate: bool,
    ) -> Result<Head> {
        let asked = self.asked();
        let (found, head, writable) = match self.remote.open(path.clone()).await {
            Err(err) if err.is_offline() => {
                return match self.reader.known(entry) {
                    Some((_, false)) if write => Err(Errno::EACCES.into()),
                    Some((file, _)) => Ok(file),
                    None if truncate => Ok(None),
                    None => Err(err),
                };
            }
            opened => opened.map_err(|err| self.gone(path, err))?,
        };
        self.remember(&found, asked);
        // The name now holds another entry, which the kernel looks up again (C7).
        if found.id != entry {
            return Err(Errno::ESTALE.into());
        }
        let open = |entry: &EntryId| self.journal.is_open(entry);
        if self.reader.follow(entry, (head, writable), open)
            && let Some(dropping) = self.invalidate(entry)
        {
            let _ = dropping.await;
        }
        if write && !writable {
            return Err(Errno::EACCES.into());
        }
        Ok(self.reader.current(entry).flatten())
    }
}

//! How writes reach an entry's working copy, which a seal may have taken since the last one.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use herdfs_core::proto::{EntryId, Version};

use super::{Journal, open_copy, write_record};
use crate::lock;

/// A file being filled to become a working copy; deleted unless installed.
pub struct Scratch {
    path: PathBuf,
    pub file: File,
}

impl Scratch {
    fn rename_to(self, path: &Path) -> io::Result<()> {
        fs::rename(&self.path, path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Journal {
    pub fn scratch(&self, entry: EntryId) -> io::Result<Scratch> {
        let path = self
            .dir
            .join(format!("{}.{:x}.tmp", entry.0, rand::random::<u64>()));
        let file = File::create(&path)?;
        Ok(Scratch { path, file })
    }

    /// Makes `scratch` the entry's working copy over `base`, unless another open got there first.
    pub fn install(&self, entry: EntryId, scratch: Scratch, base: Option<&Version>) -> io::Result<()> {
        let mut entries = lock(&self.entries);
        let tracked = entries.entry(entry).or_default();
        if tracked.working.is_none() && tracked.sealed.is_none() {
            scratch.rename_to(&self.working_path(entry))?;
            tracked.working = Some(Arc::new(open_copy(&self.working_path(entry))?));
            tracked.base = base.cloned();
        }
        Ok(())
    }

    /// Joins the entry's journal copies for writing, emptying them if asked. False if it has none.
    pub fn attach(&self, entry: EntryId, truncate: bool) -> io::Result<bool> {
        if self.changes(entry).is_none() {
            return Ok(false);
        }
        if truncate {
            self.set_len(entry, 0)?;
        }
        Ok(true)
    }

    pub fn write(&self, entry: EntryId, off: u64, data: &[u8]) -> io::Result<()> {
        self.change(entry, false, |copy| copy.write_all_at(data, off))
    }

    pub fn set_len(&self, entry: EntryId, size: u64) -> io::Result<()> {
        self.change(entry, size == 0, |copy| copy.set_len(size))
    }

    /// Changes the working copy, first starting one if a seal took the last. Runs under the
    /// lock, so a seal never takes a copy halfway through a change (C3).
    fn change(
        &self,
        entry: EntryId,
        empty: bool,
        change: impl FnOnce(&File) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut entries = lock(&self.entries);
        let tracked = entries
            .get_mut(&entry)
            .ok_or_else(|| io::Error::other("no journal copy"))?;
        if tracked.working.is_none() {
            let sealed = tracked.sealed.as_ref().map(|(seal, _)| *seal);
            tracked.working = Some(self.restart(entry, sealed, empty)?);
        }
        // A change after a set modification time makes it the time of the change.
        tracked.mtime = None;
        tracked.working.as_deref().map_or(Ok(()), change)
    }

    /// Records a modification time for the entry's next commit to keep, if a commit will
    /// come that has not started yet. False otherwise: the caller sets it at the authority.
    pub fn set_mtime(&self, entry: EntryId, mtime_ms: i64) -> io::Result<bool> {
        let mut entries = lock(&self.entries);
        let Some(tracked) = entries.get_mut(&entry) else {
            return Ok(false);
        };
        let time = UNIX_EPOCH + Duration::from_millis(mtime_ms.try_into().unwrap_or(0));
        if let Some(working) = &tracked.working {
            working.set_modified(time)?;
            tracked.mtime = Some(mtime_ms);
            return Ok(true);
        }
        let Some(record) = tracked
            .pending
            .as_mut()
            .filter(|record| tracked.reading != Some(record.seal))
        else {
            return Ok(false);
        };
        record.mtime_ms = Some(mtime_ms);
        write_record(&self.record_path(entry), record)?;
        if let Some((_, sealed)) = &tracked.sealed {
            sealed.set_modified(time)?;
        }
        Ok(true)
    }

    /// A new working copy after a seal: empty if `empty`, else a copy of the sealed one.
    fn restart(&self, entry: EntryId, sealed: Option<u64>, empty: bool) -> io::Result<Arc<File>> {
        let seal = sealed.ok_or_else(|| io::Error::other("no journal copy"))?;
        let path = self.working_path(entry);
        if empty {
            File::create(&path)?;
        } else {
            fs::copy(self.sealed_path(entry, seal), &path)?;
        }
        Ok(Arc::new(open_copy(&path)?))
    }
}

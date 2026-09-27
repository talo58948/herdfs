//! How writes reach an entry's working copy, which a seal may have taken since the last one.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use herdfs_core::proto::{EntryId, Version};

use super::{Journal, chunk_ids, open_copy};
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
            tracked.base = base.map(|version| version.id);
            tracked.base_chunks = base.map_or_else(Vec::new, chunk_ids);
        }
        Ok(())
    }

    /// Joins the entry's journal copies for writing, emptying them if asked. False if it has none.
    pub fn attach(&self, entry: EntryId, truncate: bool) -> io::Result<bool> {
        if self.copy(entry).is_none() {
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
        tracked.working.as_deref().map_or(Ok(()), change)
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

//! How writes reach an entry's working copy, which a seal may have taken since the last one.

use std::fs::{File, Permissions};
use std::io;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::sync::Arc;

use herdfs_core::proto::{EntryId, Version};
use tempfile::{Builder, NamedTempFile};

use super::{Journal, write_record};

impl Journal {
    /// A file to fill as the entry's next working copy, deleted unless installed. Its mode is
    /// the one `File::create` gives the other journal files.
    pub fn scratch(&self, entry: EntryId) -> io::Result<NamedTempFile> {
        Builder::new()
            .prefix(&format!("{}.", entry.0))
            .suffix(".tmp")
            .permissions(Permissions::from_mode(0o666))
            .tempfile_in(&self.dir)
    }

    /// Makes `scratch` the entry's working copy over `base`, unless another open got there first.
    pub fn install(&self, entry: EntryId, scratch: NamedTempFile, base: Option<&Version>) -> io::Result<()> {
        let installed = self.local(entry, true, |local| {
            if local.working.is_none() && local.pending.is_none() {
                local.working = Some(Arc::new(scratch.persist(self.working_path(entry))?));
                local.base = base.cloned();
            }
            Ok(())
        });
        installed.unwrap_or(Ok(()))
    }

    /// Joins the entry's journal copies for writing, emptying them if asked. False if it has none.
    pub fn attach(&self, entry: EntryId, truncate: bool) -> io::Result<bool> {
        let attached = self.changes(entry).is_some();
        if attached && truncate {
            self.set_len(entry, 0)?;
        }
        Ok(attached)
    }

    pub fn write(&self, entry: EntryId, off: u64, data: &[u8]) -> io::Result<()> {
        self.change(entry, false, |copy| copy.write_all_at(data, off))
    }

    pub fn set_len(&self, entry: EntryId, size: u64) -> io::Result<()> {
        self.change(entry, size == 0, |copy| copy.set_len(size))
    }

    /// Changes the working copy, first starting one if a seal took the last. Runs under the
    /// inode's lock, so a seal never takes a copy halfway through a change (C3).
    fn change(
        &self,
        entry: EntryId,
        empty: bool,
        change: impl FnOnce(&File) -> io::Result<()>,
    ) -> io::Result<()> {
        let no_copy = || io::Error::other("no journal copy");
        let mut change = Some(change);
        loop {
            let restart = self.local(entry, false, |local| {
                if let Some(working) = &local.working {
                    return change
                        .take()
                        .map_or(Ok(None), |change| change(working).map(|()| None));
                }
                let seal = local.pending.as_ref().ok_or_else(no_copy)?.0.seal;
                // Opened under the lock, before a commit can delete it.
                let sealed = (!empty).then(|| File::open(self.sealed_path(entry, seal)));
                Ok(Some((seal, sealed.transpose()?)))
            });
            match restart.ok_or_else(no_copy)?? {
                Some((seal, sealed)) => self.restart(entry, seal, sealed)?,
                None => return Ok(()),
            }
        }
    }

    /// Records a modification time for the entry's next commit to keep, if a commit will
    /// come that has not started yet. False otherwise: the caller sets it at the authority.
    pub fn set_mtime(&self, entry: EntryId, mtime_ms: i64) -> io::Result<bool> {
        let time = crate::from_unix_ms(mtime_ms);
        let set = self.local(entry, false, |local| {
            // The next seal takes the working copy's modification time.
            if let Some(working) = &local.working {
                working.set_modified(time)?;
                return Ok(true);
            }
            let reading = local.reading;
            let pending = local.pending.as_mut();
            let Some((record, sealed)) = pending.filter(|(record, _)| reading != Some(record.seal)) else {
                return Ok(false);
            };
            record.mtime_ms = Some(mtime_ms);
            write_record(&self.record_path(entry), record)?;
            sealed.set_modified(time)?;
            Ok(true)
        });
        set.unwrap_or(Ok(false))
    }

    /// Starts a working copy after seal `seal`: a copy of `sealed`, else empty. The copy is made
    /// outside the lock, so writes to other entries need not wait for it, and is dropped if
    /// another write started a copy or sealed a newer one meanwhile.
    fn restart(&self, entry: EntryId, seal: u64, sealed: Option<File>) -> io::Result<()> {
        let mut scratch = self.scratch(entry)?;
        if let Some(mut sealed) = sealed {
            io::copy(&mut sealed, scratch.as_file_mut())?;
        }
        let installed = self.local(entry, false, |local| {
            let latest = local.pending.as_ref().map(|(record, _)| record.seal);
            if local.working.is_none() && latest.is_none_or(|latest| latest == seal) {
                local.working = Some(Arc::new(scratch.persist(self.working_path(entry))?));
            }
            Ok(())
        });
        installed.unwrap_or(Ok(()))
    }
}

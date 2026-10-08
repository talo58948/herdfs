//! One entry's changes on this client, held in its inode (fs/inode.rs) while it has some or a
//! handle is open, and the operations on them. Each runs under the inode's lock alone, so a
//! seal never takes a copy halfway through a change (C3) and journal I/O on one file waits for
//! no other.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use herdfs_core::proto::{EntryId, Version};
use tempfile::NamedTempFile;
use tokio::sync::watch;

use super::{Journal, LONGEST_QUIET, QUIET, Record, write_record};

#[derive(Default)]
pub struct Local {
    handles: usize,
    /// What handles write to, from the first write after a seal to the next seal.
    working: Option<Arc<File>>,
    /// The version the copies started from, then each version they committed.
    pub(super) base: Option<Version>,
    /// The latest seal the committer has not settled, which newer seals replace, and its sealed
    /// copy, which reads see without a working copy.
    pub(super) pending: Option<(Record, Arc<File>)>,
    seals: u64,
    pub(super) committing: bool,
    /// Until when the committer waits for a newer seal before committing the pending one, and
    /// when it commits at the latest.
    quiet_until: Option<Instant>,
    pub(super) due: Option<Instant>,
    /// The seal the committer is reading, whose file outlives newer seals until it is done.
    pub(super) reading: Option<u64>,
    /// The seal the last commit attempt covered, and whether it committed.
    pub(super) attempts: watch::Sender<(u64, bool)>,
    /// How many commits have settled, and at which of those the authority last refused one: an
    /// fsync reports a refusal after the handle's open or last fsync (C2).
    pub(super) settles: u64,
    pub(super) refused: u64,
}

impl Local {
    /// The journal copy while it holds changes: the working copy, else the sealed one a pending
    /// commit reads. Every read on this client sees it (C1).
    pub fn copy(&self) -> Option<Arc<File>> {
        let sealed = self.pending.as_ref().map(|(_, sealed)| sealed);
        self.working.as_ref().or(sealed).cloned()
    }

    pub fn is_open(&self) -> bool {
        self.handles > 0
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn pending_bytes(&self) -> Option<u64> {
        self.pending.as_ref()?.1.metadata().ok().map(|meta| meta.len())
    }

    /// Nothing holds it any more: no handle, and nothing left to commit.
    pub fn idle(&self) -> bool {
        self.handles == 0 && self.working.is_none() && self.pending.is_none() && !self.committing
    }

    pub fn opened(&mut self) {
        self.handles += 1;
    }

    pub fn closed(&mut self) {
        self.handles = self.handles.saturating_sub(1);
    }

    /// How many commits have settled, and at which the last refusal came, or 0.
    pub fn settles(&self) -> (u64, u64) {
        (self.settles, self.refused)
    }

    /// Makes `scratch` the working copy over `base`, unless another open got there first.
    pub fn install(
        &mut self,
        journal: &Journal,
        entry: EntryId,
        scratch: NamedTempFile,
        base: Option<&Version>,
    ) -> io::Result<()> {
        if self.working.is_none() && self.pending.is_none() {
            self.working = Some(Arc::new(scratch.persist(journal.working_path(entry))?));
            self.base = base.cloned();
        }
        Ok(())
    }

    /// Changes the working copy, first starting one if a seal took the last: empty if `empty`,
    /// else a copy of the sealed one.
    pub fn change(
        &mut self,
        journal: &Journal,
        entry: EntryId,
        empty: bool,
        change: impl FnOnce(&File) -> io::Result<()>,
    ) -> io::Result<()> {
        let working = match &self.working {
            Some(working) => Arc::clone(working),
            None => self.start_working(journal, entry, empty)?,
        };
        change(&working)
    }

    fn start_working(&mut self, journal: &Journal, entry: EntryId, empty: bool) -> io::Result<Arc<File>> {
        let seal = self
            .pending
            .as_ref()
            .ok_or_else(|| io::Error::other("no journal copy"))?
            .0
            .seal;
        let mut scratch = journal.scratch(entry)?;
        if !empty {
            io::copy(
                &mut File::open(journal.sealed_path(entry, seal))?,
                scratch.as_file_mut(),
            )?;
        }
        let working = Arc::new(scratch.persist(journal.working_path(entry))?);
        self.working = Some(Arc::clone(&working));
        Ok(working)
    }

    pub fn write(&mut self, journal: &Journal, entry: EntryId, off: u64, data: &[u8]) -> io::Result<()> {
        self.change(journal, entry, false, |copy| copy.write_all_at(data, off))
    }

    pub fn set_len(&mut self, journal: &Journal, entry: EntryId, size: u64) -> io::Result<()> {
        self.change(journal, entry, size == 0, |copy| copy.set_len(size))
    }

    /// Makes the working copy durable, renames it to the next sealed file, and records that for
    /// the committer (C3, C4a). Under the inode's lock, which only this file's calls wait for.
    pub fn seal(&mut self, journal: &Journal, entry: EntryId) -> io::Result<()> {
        self.seal_as(journal, entry, false)
    }

    /// Persists a truncate-only close without letting the live committer publish it yet.
    pub fn defer(&mut self, journal: &Journal, entry: EntryId) -> io::Result<()> {
        self.seal_as(journal, entry, true)
    }

    fn seal_as(&mut self, journal: &Journal, entry: EntryId, deferred: bool) -> io::Result<()> {
        let Some(working) = self.working.clone() else {
            return Ok(());
        };
        let seal = self.seals + 1;
        working.sync_all()?;
        fs::rename(journal.working_path(entry), journal.sealed_path(entry, seal))?;
        let older = self.pending.as_ref().map(|(record, _)| record.seal);
        (self.seals, self.working) = (seal, None);
        let now = Instant::now();
        let due = *self.due.get_or_insert(now + LONGEST_QUIET);
        self.quiet_until = Some((now + QUIET).min(due));
        let record = Record {
            entry,
            seal,
            deferred,
            base: self.base.clone(),
            mtime_ms: working.metadata()?.modified().ok().map(crate::unix_ms),
            request_id: format!("{:032x}", rand::random::<u128>()),
        };
        self.queue(journal, record.clone(), working);
        // Persisted before the older sealed file goes, so a crash leaves one a record names.
        write_record(&journal.record_path(entry), &record)?;
        if let Some(older) = older.filter(|older| self.reading != Some(*older)) {
            let _ = fs::remove_file(journal.sealed_path(entry, older));
        }
        Ok(())
    }

    /// Takes a sealed write recovered from the journal after a restart (C4a).
    pub fn recovered(&mut self, journal: &Journal, mut record: Record, sealed: Arc<File>) {
        record.deferred = false;
        (self.base, self.seals) = (record.base.clone(), record.seal);
        self.queue(journal, record, sealed);
    }

    /// Makes `record` the pending write, in place of any older one, and wakes the committer.
    fn queue(&mut self, journal: &Journal, record: Record, sealed: Arc<File>) {
        let entry = record.entry;
        let deferred = record.deferred;
        self.pending = Some((record, sealed));
        if !deferred && !std::mem::replace(&mut self.committing, true) {
            journal.wake(entry);
        }
    }

    /// Lets the live committer take a truncate that flush made durable.
    pub fn activate(&mut self, journal: &Journal, entry: EntryId) -> io::Result<()> {
        let Some((record, _)) = self.pending.as_mut().filter(|(record, _)| record.deferred) else {
            return Ok(());
        };
        let active = Record {
            deferred: false,
            ..record.clone()
        };
        write_record(&journal.record_path(entry), &active)?;
        *record = active;
        if !std::mem::replace(&mut self.committing, true) {
            journal.wake(entry);
        }
        Ok(())
    }

    /// Records a modification time for the next commit to keep, if one will come that has not
    /// started yet. False otherwise: the caller sets it at the authority.
    pub fn set_mtime(&mut self, journal: &Journal, entry: EntryId, mtime_ms: i64) -> io::Result<bool> {
        let time = crate::from_unix_ms(mtime_ms);
        // The next seal takes the working copy's modification time.
        if let Some(working) = &self.working {
            working.set_modified(time)?;
            return Ok(true);
        }
        let reading = self.reading;
        let pending = self.pending.as_mut();
        let Some((record, sealed)) = pending.filter(|(record, _)| reading != Some(record.seal)) else {
            return Ok(false);
        };
        record.mtime_ms = Some(mtime_ms);
        write_record(&journal.record_path(entry), record)?;
        sealed.set_modified(time)?;
        Ok(true)
    }

    /// Drops the changes, which removing the entry made moot: deletion wins (C10). An fsync
    /// waiting for them returns, as it would for a removed local file.
    pub fn discard(&mut self, journal: &Journal, entry: EntryId) {
        if let Some((record, _)) = self.pending.take() {
            let _ = fs::remove_file(journal.record_path(entry));
            // A commit under way still reads it; settling it removes it.
            if self.reading != Some(record.seal) {
                let _ = fs::remove_file(journal.sealed_path(entry, record.seal));
            }
            self.attempts.send_replace((record.seal, true));
        }
        self.working = None;
        let _ = fs::remove_file(journal.working_path(entry));
    }

    /// Ends the quiet period: an fsync waits for the commit.
    pub fn urge(&mut self) {
        self.quiet_until = None;
    }

    /// How much longer the committer waits for a newer seal, if at all.
    pub fn quiet(&self) -> Option<Duration> {
        let left = self.quiet_until?.saturating_duration_since(Instant::now());
        Some(left).filter(|left| !left.is_zero())
    }

    /// The latest pending seal and a feed of commit attempts, for fsync to wait on.
    pub fn pending(&self) -> Option<(u64, watch::Receiver<(u64, bool)>)> {
        Some((self.pending.as_ref()?.0.seal, self.attempts.subscribe()))
    }
}

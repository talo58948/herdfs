//! Journal copies of files being written, and the records that carry a closed write
//! through a crash until the authority commits it (C1, C3, C4a, C12).
//!
//! Handles write to an entry's working copy. Sealing renames it to a sealed file that never
//! changes again, so a commit always holds one whole write (C3); later writes start a new
//! working copy.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use herdfs_core::proto::{EntryId, Version};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc, watch};

use crate::fs::Inodes;

mod commit;
mod working;

pub use commit::Committer;

const REJECTED: &str = "rejected";
/// How long a seal waits for a newer one before its commit starts, so a burst of closes commits
/// once; but never longer than `LONGEST_QUIET` after the oldest seal not yet committing.
const QUIET: Duration = Duration::from_millis(250);
const LONGEST_QUIET: Duration = Duration::from_secs(1);

/// A sealed write: what the committer needs, persisted next to the sealed file it names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub entry: EntryId,
    pub seal: u64,
    pub base: Option<Version>,
    /// The file's modification time, which the commit keeps: the time of its last write on
    /// this client, or one set on it (`cp -p`).
    pub mtime_ms: Option<i64>,
    /// Lets the authority answer a retried commit with the first result.
    pub request_id: String,
}

pub struct Journal {
    dir: PathBuf,
    /// Where each entry's local state lives, in its inode (fs/inode.rs).
    inodes: Arc<Inodes>,
    wake: mpsc::UnboundedSender<EntryId>,
    /// Wakes committers waiting out a quiet period when an fsync wants its commit now.
    urged: Notify,
}

/// This client's changes to one entry, held in its inode while it has some or a handle is open.
#[derive(Default)]
pub struct Local {
    handles: usize,
    /// What handles write to, from the first write after a seal to the next seal.
    working: Option<Arc<File>>,
    /// The version the copies started from, then each version they committed.
    base: Option<Version>,
    /// The latest seal the committer has not settled, which newer seals replace, and its sealed
    /// copy, which reads see without a working copy.
    pending: Option<(Record, Arc<File>)>,
    seals: u64,
    committing: bool,
    /// Until when the committer waits for a newer seal before committing the pending one, and
    /// when it commits at the latest.
    quiet_until: Option<Instant>,
    due: Option<Instant>,
    /// The seal the committer is reading, whose file outlives newer seals until it is done.
    reading: Option<u64>,
    /// The seal the last commit attempt covered, and whether it committed.
    attempts: watch::Sender<(u64, bool)>,
    /// How many commits have settled, and at which of those the authority last refused one: an
    /// fsync reports a refusal after the handle's open or last fsync (C2).
    settles: u64,
    refused: u64,
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

    fn idle(&self) -> bool {
        self.handles == 0 && self.pending.is_none() && !self.committing
    }
}

impl Journal {
    /// Opens the journal in `dir` and queues the sealed write each record names (C4a). Other
    /// copies were never closed, so nothing was promised for them.
    pub fn open(dir: PathBuf, inodes: Arc<Inodes>) -> io::Result<(Self, mpsc::UnboundedReceiver<EntryId>)> {
        fs::create_dir_all(dir.join(REJECTED))?;
        let (wake, woken) = mpsc::unbounded_channel();
        let urged = Notify::new();
        let journal = Self {
            dir,
            inodes,
            wake,
            urged,
        };
        let paths = fs::read_dir(&journal.dir)?.map(|item| Ok(item?.path()));
        let paths: Vec<PathBuf> = paths.collect::<io::Result<_>>()?;
        let is_record = |path: &&PathBuf| path.extension() == Some(OsStr::new("json"));
        let mut kept = HashSet::new();
        for record in paths.iter().filter(is_record) {
            kept.extend(journal.recover(record)?);
        }
        let stray = |path: &&PathBuf| path.is_file() && !kept.contains(*path);
        paths.iter().filter(stray).try_for_each(fs::remove_file)?;
        Ok((journal, woken))
    }

    /// Queues the write a record names, and returns the files to keep for it. A record whose
    /// sealed copy is gone was settled when a crash cut its removal short.
    fn recover(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let record: Record = serde_json::from_slice(&fs::read(path)?)?;
        let sealed = self.sealed_path(record.entry, record.seal);
        let file = match File::open(&sealed) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(record = %path.display(), "dropping a settled commit's record");
                return Ok(Vec::new());
            }
            file => Arc::new(file?),
        };
        self.local(record.entry, true, |local| {
            (local.base, local.seals) = (record.base.clone(), record.seal);
            self.queue(local, record, file);
        });
        Ok(vec![path.to_owned(), sealed])
    }

    /// Runs `change` on the entry's local state, made first with `make`, under its inode's lock
    /// alone; then forgets that state, with its working copy, once no handle holds it and nothing
    /// is left to commit. `None` if the entry has none.
    fn local<T>(&self, entry: EntryId, make: bool, change: impl FnOnce(&mut Local) -> T) -> Option<T> {
        self.inodes
            .local(entry, make, |state| {
                let local = state.local.as_mut()?;
                let changed = change(local);
                if local.idle() {
                    let _ = fs::remove_file(self.working_path(entry));
                    state.local = None;
                }
                Some(changed)
            })
            .flatten()
    }

    pub fn inodes(&self) -> Arc<Inodes> {
        self.inodes.clone()
    }

    /// How many commits of the entry have settled, and at which the last refusal came, or 0.
    pub fn settles(&self, entry: EntryId) -> (u64, u64) {
        let settles = self.local(entry, false, |local| (local.settles, local.refused));
        settles.unwrap_or((0, 0))
    }

    /// Counts a handle opened on the entry, which keeps its local state until `closed`.
    pub fn opened(&self, entry: EntryId) {
        self.local(entry, true, |local| local.handles += 1);
    }

    pub fn closed(&self, entry: EntryId) {
        self.local(entry, false, |local| {
            local.handles = local.handles.saturating_sub(1);
        });
    }

    /// The entry's journal copy while it holds changes (C1).
    pub fn changes(&self, entry: EntryId) -> Option<Arc<File>> {
        self.local(entry, false, |local| local.copy()).flatten()
    }

    /// Makes the working copy durable, renames it to the next sealed file, and records that
    /// for the committer (C3, C4a).
    pub fn seal(&self, entry: EntryId) -> io::Result<()> {
        let working = self.local(entry, false, |local| local.working.clone()).flatten();
        let Some(working) = working else {
            return Ok(());
        };
        // The bulk of the flush, outside the lock; the second sync below only catches writes
        // that land before the rename.
        working.sync_all()?;
        let sealed = self.local(entry, false, |local| {
            // Otherwise another handle sealed this copy meanwhile, and every write made to it.
            let sealing = local
                .working
                .as_ref()
                .is_some_and(|copy| Arc::ptr_eq(copy, &working));
            sealing.then(|| self.sealed(entry, local, working))
        });
        sealed.flatten().unwrap_or(Ok(()))
    }

    fn sealed(&self, entry: EntryId, local: &mut Local, working: Arc<File>) -> io::Result<()> {
        let seal = local.seals + 1;
        fs::rename(self.working_path(entry), self.sealed_path(entry, seal))?;
        working.sync_all()?;
        let older = local.pending.as_ref().map(|(record, _)| record.seal);
        (local.seals, local.working) = (seal, None);
        let now = Instant::now();
        let due = *local.due.get_or_insert(now + LONGEST_QUIET);
        local.quiet_until = Some((now + QUIET).min(due));
        let record = Record {
            entry,
            seal,
            base: local.base.clone(),
            mtime_ms: working.metadata()?.modified().ok().map(crate::unix_ms),
            request_id: format!("{:032x}", rand::random::<u128>()),
        };
        self.queue(local, record.clone(), working);
        // Persisted before the older sealed file goes, so a crash leaves one a record names.
        write_record(&self.record_path(entry), &record)?;
        if let Some(older) = older.filter(|older| local.reading != Some(*older)) {
            let _ = fs::remove_file(self.sealed_path(entry, older));
        }
        Ok(())
    }

    /// Makes `record` the pending write, in place of any older one, and wakes the committer.
    fn queue(&self, local: &mut Local, record: Record, sealed: Arc<File>) {
        let entry = record.entry;
        local.pending = Some((record, sealed));
        if !std::mem::replace(&mut local.committing, true) {
            // Fails only once the committer has stopped, at shutdown.
            let _ = self.wake.send(entry);
        }
    }

    /// Drops the entry's changes, which removing it made moot: deletion wins (C10). An fsync
    /// waiting for them returns, as it would for a removed local file.
    pub fn discard(&self, entry: EntryId) {
        self.local(entry, false, |local| {
            if let Some((record, _)) = local.pending.take() {
                let _ = fs::remove_file(self.record_path(entry));
                // A commit under way still reads it; settling it removes it.
                if local.reading != Some(record.seal) {
                    let _ = fs::remove_file(self.sealed_path(entry, record.seal));
                }
                local.attempts.send_replace((record.seal, true));
            }
            local.working = None;
            let _ = fs::remove_file(self.working_path(entry));
        });
    }

    fn working_path(&self, entry: EntryId) -> PathBuf {
        self.dir.join(format!("{}.work", entry.0))
    }

    fn sealed_path(&self, entry: EntryId, seal: u64) -> PathBuf {
        self.dir.join(format!("{}.{seal}.sealed", entry.0))
    }

    fn record_path(&self, entry: EntryId) -> PathBuf {
        self.dir.join(format!("{}.json", entry.0))
    }
}

fn write_record(path: &Path, record: &Record) -> io::Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut file = File::create(&temp)?;
    file.write_all(&serde_json::to_vec(record)?)?;
    file.sync_all()?;
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests;

//! Journal copies of files being written, and the records that carry a closed write
//! through a crash until the authority commits it (C1, C3, C4a, C12).
//!
//! Handles write to an entry's working copy. Sealing renames it to a sealed file that never
//! changes again, so a commit always holds one whole write (C3); later writes start a new
//! working copy.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use herdfs_core::proto::{EntryId, Version};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc, watch};

use crate::lock;

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
    entries: Mutex<HashMap<EntryId, Tracked>>,
    wake: mpsc::UnboundedSender<EntryId>,
    /// Counts settled commits, so cached remote state from before one can be told apart.
    settled: AtomicU64,
    /// Wakes committers waiting out a quiet period when an fsync wants its commit now.
    urged: Notify,
}

/// An entry that has open handles or journal copies.
#[derive(Default)]
struct Tracked {
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
}

/// Keeps an entry's journal copies alive while one of its handles is open.
pub struct Registration(Arc<Journal>, EntryId);

impl Drop for Registration {
    fn drop(&mut self) {
        self.0.closed(self.1);
    }
}

impl Journal {
    /// Opens the journal in `dir` and queues the sealed write each record names (C4a). Other
    /// copies were never closed, so nothing was promised for them.
    pub fn open(dir: PathBuf) -> io::Result<(Self, mpsc::UnboundedReceiver<EntryId>)> {
        fs::create_dir_all(dir.join(REJECTED))?;
        let (wake, woken) = mpsc::unbounded_channel();
        let journal = Self {
            dir,
            entries: Mutex::default(),
            wake,
            settled: AtomicU64::default(),
            urged: Notify::new(),
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
        let mut entries = lock(&self.entries);
        let tracked = entries.entry(record.entry).or_default();
        (tracked.base, tracked.seals) = (record.base.clone(), record.seal);
        self.queue(tracked, record, file);
        Ok(vec![path.to_owned(), sealed])
    }

    /// Whether a handle has the entry open.
    pub fn is_open(&self, entry: &EntryId) -> bool {
        lock(&self.entries)
            .get(entry)
            .is_some_and(|tracked| tracked.handles > 0)
    }

    pub fn opened(self: &Arc<Self>, entry: EntryId) -> Registration {
        lock(&self.entries).entry(entry).or_default().handles += 1;
        Registration(self.clone(), entry)
    }

    fn closed(&self, entry: EntryId) {
        let mut entries = lock(&self.entries);
        if let Some(tracked) = entries.get_mut(&entry) {
            tracked.handles -= 1;
        }
        self.tidy(&mut entries, entry);
    }

    /// The entry's journal copy while it holds changes: the working copy, else the sealed one
    /// a pending commit reads. Every read on this client sees it (C1).
    pub fn changes(&self, entry: EntryId) -> Option<Arc<File>> {
        let entries = lock(&self.entries);
        let tracked = entries.get(&entry)?;
        let sealed = tracked.pending.as_ref().map(|(_, sealed)| sealed);
        tracked.working.as_ref().or(sealed).cloned()
    }

    pub fn settled(&self) -> u64 {
        self.settled.load(Relaxed)
    }

    /// Makes the working copy durable, renames it to the next sealed file, and records that
    /// for the committer (C3, C4a).
    pub fn seal(&self, entry: EntryId) -> io::Result<()> {
        let working = lock(&self.entries).get(&entry).and_then(|t| t.working.clone());
        let Some(working) = working else {
            return Ok(());
        };
        // The bulk of the flush, outside the lock; the second sync below only catches writes
        // that land before the rename.
        working.sync_all()?;
        let mut entries = lock(&self.entries);
        let sealing =
            |tracked: &&mut Tracked| tracked.working.as_ref().is_some_and(|w| Arc::ptr_eq(w, &working));
        // Otherwise another handle sealed this copy meanwhile, and every write made to it.
        let Some(tracked) = entries.get_mut(&entry).filter(sealing) else {
            return Ok(());
        };
        let seal = tracked.seals + 1;
        fs::rename(self.working_path(entry), self.sealed_path(entry, seal))?;
        working.sync_all()?;
        let older = tracked.pending.as_ref().map(|(record, _)| record.seal);
        (tracked.seals, tracked.working) = (seal, None);
        let now = Instant::now();
        let due = *tracked.due.get_or_insert(now + LONGEST_QUIET);
        tracked.quiet_until = Some((now + QUIET).min(due));
        let record = Record {
            entry,
            seal,
            base: tracked.base.clone(),
            mtime_ms: working.metadata()?.modified().ok().map(crate::unix_ms),
            request_id: format!("{:032x}", rand::random::<u128>()),
        };
        self.queue(tracked, record.clone(), working);
        // Persisted before the older sealed file goes, so a crash leaves one a record names.
        write_record(&self.record_path(entry), &record)?;
        if let Some(older) = older.filter(|older| tracked.reading != Some(*older)) {
            let _ = fs::remove_file(self.sealed_path(entry, older));
        }
        Ok(())
    }

    /// Makes `record` the pending write, in place of any older one, and wakes the committer.
    fn queue(&self, tracked: &mut Tracked, record: Record, sealed: Arc<File>) {
        let entry = record.entry;
        tracked.pending = Some((record, sealed));
        if !std::mem::replace(&mut tracked.committing, true) {
            // Fails only once the committer has stopped, at shutdown.
            let _ = self.wake.send(entry);
        }
    }

    /// Drops the entry's changes, which removing it made moot: deletion wins (C10). An fsync
    /// waiting for them returns, as it would for a removed local file.
    pub fn discard(&self, entry: EntryId) {
        let mut entries = lock(&self.entries);
        let Some(tracked) = entries.get_mut(&entry) else {
            return;
        };
        if let Some((record, _)) = tracked.pending.take() {
            let _ = fs::remove_file(self.record_path(entry));
            // A commit under way still reads it; settling it removes it.
            if tracked.reading != Some(record.seal) {
                let _ = fs::remove_file(self.sealed_path(entry, record.seal));
            }
            tracked.attempts.send_replace((record.seal, true));
        }
        tracked.working = None;
        let _ = fs::remove_file(self.working_path(entry));
        self.tidy(&mut entries, entry);
    }

    /// Forgets an entry nobody has open once nothing is left to commit, with its working copy.
    fn tidy(&self, entries: &mut HashMap<EntryId, Tracked>, entry: EntryId) {
        let idle = |t: &Tracked| t.handles == 0 && t.pending.is_none() && !t.committing;
        if entries.get(&entry).is_some_and(idle) {
            let _ = fs::remove_file(self.working_path(entry));
            entries.remove(&entry);
        }
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

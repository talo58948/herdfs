//! Journal copies of files being written, and the records that carry a closed write
//! through a crash until the authority commits it (C1, C3, C4a, C12).
//!
//! Handles write to an entry's working copy. Sealing renames it to a sealed file that never
//! changes again, so a commit always holds one whole write (C3); later writes start a new
//! working copy.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use herdfs_core::proto::{EntryId, Version, VersionId};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::lock;

mod commit;
mod working;

pub use commit::Committer;

const REJECTED: &str = "rejected";

/// A sealed write: what the committer needs, persisted next to the sealed file it names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub entry: EntryId,
    pub seal: u64,
    pub base: Option<Version>,
    /// A modification time set on the file, which the commit keeps (`cp -p`).
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
}

/// An entry that has open handles or journal copies.
#[derive(Default)]
struct Tracked {
    handles: usize,
    /// What handles write to, from the first write after a seal to the next seal.
    working: Option<Arc<File>>,
    /// The latest sealed copy, while its commit is pending; reads see it without a working copy.
    sealed: Option<(u64, Arc<File>)>,
    /// The version the copies started from, then each version they committed.
    base: Option<Version>,
    /// A modification time set since the last seal, for the next one to keep.
    mtime: Option<i64>,
    /// The latest seal the committer has not settled; newer seals replace it.
    pending: Option<Record>,
    seals: u64,
    committing: bool,
    /// The seal the committer is reading, whose file outlives newer seals until it is done.
    reading: Option<u64>,
    /// The seal the last commit attempt covered, and whether it committed.
    attempts: watch::Sender<(u64, bool)>,
}

/// Keeps an entry's journal copies alive while one of its handles is open.
pub struct Registration {
    journal: Arc<Journal>,
    entry: EntryId,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.journal.closed(self.entry);
    }
}

impl Journal {
    /// Opens the journal in `dir` and queues the sealed write each record names (C4a). Other
    /// copies were never closed, so nothing was promised for them.
    pub fn open(dir: PathBuf) -> io::Result<(Self, mpsc::UnboundedReceiver<EntryId>)> {
        fs::create_dir_all(dir.join(REJECTED))?;
        let (wake, woken) = mpsc::unbounded_channel();
        let (entries, settled) = (Mutex::default(), AtomicU64::default());
        let journal = Self {
            dir,
            entries,
            wake,
            settled,
        };
        let paths = fs::read_dir(&journal.dir)?.map(|item| Ok(item?.path()));
        let paths: Vec<PathBuf> = paths.collect::<io::Result<_>>()?;
        let records = paths
            .iter()
            .filter(|path| path.extension() == Some(OsStr::new("json")));
        let kept = records
            .map(|path| journal.recover(path))
            .collect::<io::Result<Vec<_>>>()?;
        let kept: HashSet<&PathBuf> = kept.iter().flatten().collect();
        for path in paths.iter().filter(|path| path.is_file() && !kept.contains(path)) {
            fs::remove_file(path)?;
        }
        Ok((journal, woken))
    }

    /// Queues the write a record names, and returns the files to keep for it.
    fn recover(&self, path: &Path) -> io::Result<[PathBuf; 2]> {
        let record: Record = serde_json::from_slice(&fs::read(path)?)?;
        let sealed = self.sealed_path(record.entry, record.seal);
        let file = Arc::new(File::open(&sealed)?);
        let mut entries = lock(&self.entries);
        let tracked = entries.entry(record.entry).or_default();
        tracked.base = record.base.clone();
        (tracked.seals, tracked.sealed) = (record.seal, Some((record.seal, file)));
        self.queue(tracked, record);
        Ok([path.to_owned(), sealed])
    }

    pub fn opened(self: &Arc<Self>, entry: EntryId) -> Registration {
        lock(&self.entries).entry(entry).or_default().handles += 1;
        let journal = self.clone();
        Registration { journal, entry }
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
        tracked
            .working
            .clone()
            .or_else(|| Some(tracked.sealed.as_ref()?.1.clone()))
    }

    /// The version the entry's copies started from or last committed, if newer than `known`:
    /// what reads see once this client's changes are committed (C1).
    pub fn base_after(&self, entry: EntryId, known: Option<VersionId>) -> Option<Version> {
        let entries = lock(&self.entries);
        let base = entries.get(&entry)?.base.as_ref()?;
        (Some(base.id) > known).then(|| base.clone())
    }

    pub fn settled(&self) -> u64 {
        self.settled.load(Relaxed)
    }

    /// Makes the working copy durable, renames it to the next sealed file, and records that
    /// for the committer (C3, C4a).
    pub fn seal(&self, entry: EntryId) -> io::Result<()> {
        let working = lock(&self.entries)
            .get(&entry)
            .and_then(|tracked| tracked.working.clone());
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
        let older = tracked.sealed.replace((seal, working)).map(|(older, _)| older);
        (tracked.seals, tracked.working) = (seal, None);
        let request_id = format!("{:032x}", rand::random::<u128>());
        let (base, mtime_ms) = (tracked.base.clone(), tracked.mtime.take());
        let record = Record {
            entry,
            seal,
            base,
            mtime_ms,
            request_id,
        };
        self.queue(tracked, record.clone());
        // Persisted before the older sealed file goes, so a crash leaves one a record names.
        write_record(&self.record_path(entry), &record)?;
        if let Some(older) = older.filter(|older| tracked.reading != Some(*older)) {
            let _ = fs::remove_file(self.sealed_path(entry, older));
        }
        Ok(())
    }

    /// Makes `record` the pending write, in place of any older one, and wakes the committer.
    fn queue(&self, tracked: &mut Tracked, record: Record) {
        let entry = record.entry;
        tracked.pending = Some(record);
        if !std::mem::replace(&mut tracked.committing, true) {
            // Fails only once the committer has stopped, at shutdown.
            let _ = self.wake.send(entry);
        }
    }

    /// Forgets an entry nobody has open once nothing is left to commit, with its copies.
    fn tidy(&self, entries: &mut HashMap<EntryId, Tracked>, entry: EntryId) {
        let idle = |t: &Tracked| t.handles == 0 && t.pending.is_none() && !t.committing;
        let Some(tracked) = entries.get(&entry).filter(|tracked| idle(tracked)) else {
            return;
        };
        if let Some((seal, _)) = tracked.sealed {
            let _ = fs::remove_file(self.sealed_path(entry, seal));
        }
        let _ = fs::remove_file(self.working_path(entry));
        entries.remove(&entry);
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

fn open_copy(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
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

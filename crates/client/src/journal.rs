//! Journal copies of files being written, and the records that carry a closed write
//! through a crash until the authority commits it (C1, C4a, C12).

use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

use herdfs_core::{
    ChunkId,
    proto::{EntryId, Version, VersionId},
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::lock;

mod commit;

pub use commit::committer;

const REJECTED: &str = "rejected";

/// A sealed write: what the committer needs, persisted next to the copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub entry: EntryId,
    pub base: Option<VersionId>,
    pub base_chunks: Vec<ChunkId>,
    /// Lets the authority answer a retried commit with the first result.
    pub request_id: String,
}

pub struct Journal {
    dir: PathBuf,
    entries: Mutex<HashMap<EntryId, Tracked>>,
    wake: mpsc::UnboundedSender<EntryId>,
    scratches: AtomicU64,
    /// Counts settled commits, so cached remote state from before one can be told apart.
    settled: AtomicU64,
}

/// An entry that has open handles or a journal copy.
#[derive(Default)]
struct Tracked {
    handles: usize,
    copy: Option<Arc<File>>,
    /// The version the copy started from, then each version it committed.
    base: Option<VersionId>,
    base_chunks: Vec<ChunkId>,
    /// The latest seal the committer has not settled, numbered by `seals`.
    pending: Option<(u64, Record)>,
    seals: u64,
    committing: bool,
    /// The seal the last commit attempt covered, and whether it committed.
    attempts: watch::Sender<(u64, bool)>,
}

/// Keeps an entry's journal copy alive while one of its handles is open.
pub struct Registration {
    journal: Arc<Journal>,
    entry: EntryId,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.journal.closed(self.entry);
    }
}

/// A file being filled to become a journal copy; deleted unless installed.
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
    /// Opens the journal in `dir` and queues every sealed write a previous run left (C4a).
    pub fn open(dir: PathBuf) -> io::Result<(Self, mpsc::UnboundedReceiver<EntryId>)> {
        fs::create_dir_all(dir.join(REJECTED))?;
        let (wake, woken) = mpsc::unbounded_channel();
        let (entries, scratches, settled) = (Mutex::default(), AtomicU64::default(), AtomicU64::default());
        let journal = Self {
            dir,
            entries,
            wake,
            scratches,
            settled,
        };
        for item in fs::read_dir(&journal.dir)? {
            let path = item?.path();
            match path.extension().and_then(OsStr::to_str) {
                Some("json") => journal.recover(&path)?,
                // A copy without a record was never closed, so nothing was promised for it.
                Some("copy") if !path.with_extension("json").exists() => fs::remove_file(&path)?,
                Some("tmp") => fs::remove_file(&path)?,
                _ => {}
            }
        }
        Ok((journal, woken))
    }

    fn recover(&self, path: &Path) -> io::Result<()> {
        let record: Record = serde_json::from_slice(&fs::read(path)?)?;
        let copy = open_copy(&self.copy_path(record.entry))?;
        let mut entries = lock(&self.entries);
        let tracked = entries.entry(record.entry).or_default();
        tracked.copy = Some(Arc::new(copy));
        (tracked.base, tracked.base_chunks) = (record.base, record.base_chunks.clone());
        self.mark_pending(tracked, record);
        Ok(())
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

    /// The entry's journal copy, which every handle reads while it exists (C1).
    pub fn copy(&self, entry: EntryId) -> Option<Arc<File>> {
        lock(&self.entries).get(&entry)?.copy.clone()
    }

    /// Joins the entry's journal copy, truncating it if asked. False if it has none.
    pub fn attach(&self, entry: EntryId, truncate: bool) -> io::Result<bool> {
        let Some(copy) = self.copy(entry) else {
            return Ok(false);
        };
        if truncate {
            copy.set_len(0)?;
        }
        Ok(true)
    }

    pub fn settled(&self) -> u64 {
        self.settled.load(Relaxed)
    }

    pub fn scratch(&self, entry: EntryId) -> io::Result<Scratch> {
        let number = self.scratches.fetch_add(1, Relaxed);
        let path = self.dir.join(format!("{}.{number}.tmp", entry.0));
        let file = File::create(&path)?;
        Ok(Scratch { path, file })
    }

    /// Makes `scratch` the entry's journal copy over `base`, unless another open got there first.
    pub fn install(&self, entry: EntryId, scratch: Scratch, base: Option<&Version>) -> io::Result<()> {
        let mut entries = lock(&self.entries);
        let tracked = entries.entry(entry).or_default();
        if tracked.copy.is_none() {
            scratch.rename_to(&self.copy_path(entry))?;
            tracked.copy = Some(Arc::new(open_copy(&self.copy_path(entry))?));
            tracked.base = base.map(|version| version.id);
            tracked.base_chunks = base.map_or_else(Vec::new, chunk_ids);
        }
        Ok(())
    }

    /// Makes the copy durable and records it for the committer (C4a).
    pub fn seal(&self, entry: EntryId) -> io::Result<()> {
        let unsealable = || io::Error::other("no journal copy to seal");
        self.copy(entry).ok_or_else(unsealable)?.sync_all()?;
        let mut entries = lock(&self.entries);
        let tracked = entries.get_mut(&entry).ok_or_else(unsealable)?;
        let request_id = format!("{:032x}", rand::random::<u128>());
        let (base, base_chunks) = (tracked.base, tracked.base_chunks.clone());
        let record = Record {
            entry,
            base,
            base_chunks,
            request_id,
        };
        write_record(&self.record_path(entry), &record)?;
        self.mark_pending(tracked, record);
        Ok(())
    }

    fn mark_pending(&self, tracked: &mut Tracked, record: Record) {
        let entry = record.entry;
        tracked.seals += 1;
        tracked.pending = Some((tracked.seals, record));
        if !std::mem::replace(&mut tracked.committing, true) {
            // Fails only once the committer has stopped, at shutdown.
            let _ = self.wake.send(entry);
        }
    }

    /// Forgets an entry nobody has open, deleting its copy once nothing is left to commit.
    fn tidy(&self, entries: &mut HashMap<EntryId, Tracked>, entry: EntryId) {
        let idle = |t: &Tracked| t.handles == 0 && t.pending.is_none() && !t.committing;
        if entries.get(&entry).is_some_and(idle) && entries.remove(&entry).is_some_and(|t| t.copy.is_some()) {
            let _ = fs::remove_file(self.copy_path(entry));
        }
    }

    fn copy_path(&self, entry: EntryId) -> PathBuf {
        self.dir.join(format!("{}.copy", entry.0))
    }

    fn record_path(&self, entry: EntryId) -> PathBuf {
        self.dir.join(format!("{}.json", entry.0))
    }
}

fn open_copy(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

fn chunk_ids(version: &Version) -> Vec<ChunkId> {
    version.chunks.iter().map(|chunk| chunk.id).collect()
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

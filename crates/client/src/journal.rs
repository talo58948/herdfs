//! The journal on disk: the records that carry a closed write through a crash until the
//! authority commits it, the sealed files they name, and their recovery (C4a). What each
//! entry's changes are now is its `Local` state (journal/local.rs), held in its inode.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;

use herdfs_core::proto::{EntryId, Version};
use serde::{Deserialize, Serialize};
use tempfile::{Builder, NamedTempFile};
use tokio::sync::{Notify, mpsc};

use crate::fs::Inodes;

mod commit;
mod local;
mod rejected;

pub use commit::Committer;
pub use local::Local;

#[derive(Default)]
pub struct CommitGate {
    paused: AtomicBool,
    resumed: Notify,
}

impl CommitGate {
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        self.resumed.notify_waiters();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub async fn wait(&self) {
        while self.is_paused() {
            let resumed = self.resumed.notified();
            if self.is_paused() {
                resumed.await;
            }
        }
    }
}

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
    /// A truncate-only flush is durable but waits for release before the live committer starts.
    #[serde(default)]
    pub deferred: bool,
    pub base: Option<Version>,
    /// The file's modification time, which the commit keeps: the time of its last write on
    /// this client, or one set on it (`cp -p`).
    pub mtime_ms: Option<i64>,
    /// Lets the authority answer a retried commit with the first result.
    pub request_id: String,
}

pub struct Journal {
    dir: PathBuf,
    wake: mpsc::UnboundedSender<EntryId>,
    /// Wakes committers waiting out a quiet period when an fsync wants its commit now.
    pub urged: Notify,
}

impl Journal {
    /// Opens the journal in `dir` and queues the sealed write each record names, in the entry's
    /// inode (C4a). Other copies were never closed, so nothing was promised for them.
    pub fn open(dir: PathBuf, inodes: &Inodes) -> io::Result<(Self, mpsc::UnboundedReceiver<EntryId>)> {
        fs::create_dir_all(dir.join(REJECTED))?;
        let (wake, woken) = mpsc::unbounded_channel();
        let journal = Self {
            dir,
            wake,
            urged: Notify::new(),
        };
        let paths = fs::read_dir(&journal.dir)?.map(|item| Ok(item?.path()));
        let paths: Vec<PathBuf> = paths.collect::<io::Result<_>>()?;
        let is_record = |path: &&PathBuf| path.extension() == Some(OsStr::new("json"));
        let mut kept = HashSet::new();
        for record in paths.iter().filter(is_record) {
            kept.extend(journal.recover(record, inodes)?);
        }
        let stray = |path: &&PathBuf| path.is_file() && !kept.contains(*path);
        paths.iter().filter(stray).try_for_each(fs::remove_file)?;
        Ok((journal, woken))
    }

    /// Queues the write a record names, and returns the files to keep for it. A record whose
    /// sealed copy is gone was settled when a crash cut its removal short.
    fn recover(&self, path: &Path, inodes: &Inodes) -> io::Result<Vec<PathBuf>> {
        let record: Record = serde_json::from_slice(&fs::read(path)?)?;
        let sealed = self.sealed_path(record.entry, record.seal);
        let file = match File::open(&sealed) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(record = %path.display(), "dropping a settled commit's record");
                return Ok(Vec::new());
            }
            file => Arc::new(file?),
        };
        inodes.make_local(record.entry, |local| local.recovered(self, record, file));
        Ok(vec![path.to_owned(), sealed])
    }

    /// A file to fill as an entry's next working copy, deleted unless installed. Its mode is the
    /// one `File::create` gives the other journal files.
    pub fn scratch(&self, entry: EntryId) -> io::Result<NamedTempFile> {
        Builder::new()
            .prefix(&format!("{}.", entry.0))
            .suffix(".tmp")
            .permissions(Permissions::from_mode(0o666))
            .tempfile_in(&self.dir)
    }

    fn wake(&self, entry: EntryId) {
        // Fails only once the committer has stopped, at shutdown.
        let _ = self.wake.send(entry);
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
mod gate_tests {
    use super::CommitGate;
    use std::sync::Arc;

    // contract: U1
    #[tokio::test]
    async fn paused_commit_gate_waits_until_resume() {
        let gate = Arc::new(CommitGate::default());
        gate.pause();
        let waiting = tokio::spawn({
            let gate = gate.clone();
            async move { gate.wait().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        gate.resume();
        waiting.await.unwrap();
    }
}

#[cfg(test)]
mod tests;

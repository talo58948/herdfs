//! The committer: commits sealed journal copies in the background, retrying until the
//! authority answers (C4a, C12). Each entry gets its own task, so one slow file does not
//! hold up the others.

use std::{
    collections::HashSet,
    fs::{self, File},
    io,
    path::PathBuf,
    sync::{Arc, atomic::Ordering::Relaxed},
    time::Duration,
};

use bytes::Bytes;
use herdfs_core::{
    ChunkId, chunk,
    proto::{EntryId, Version},
};
use tokio::sync::{mpsc, watch};

use super::{Journal, REJECTED, Record, chunk_ids, write_record};
use crate::{
    error::Error,
    lock,
    remote::Remote,
    stats::{Stats, bump},
};

const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(30);

/// The latest sealed write of an entry, to be committed.
pub struct Job {
    pub seal: u64,
    pub record: Record,
    pub copy: PathBuf,
}

pub enum Outcome {
    Committed(Version),
    Rejected,
    Retrying,
}

/// Starts a commit task for each entry the journal wakes.
pub async fn committer(
    journal: Arc<Journal>,
    remote: Arc<Remote>,
    stats: Arc<Stats>,
    mut woken: mpsc::UnboundedReceiver<EntryId>,
) {
    while let Some(entry) = woken.recv().await {
        tokio::spawn(commit(journal.clone(), remote.clone(), stats.clone(), entry));
    }
}

/// Commits the entry's latest sealed write until none is left. Network failures back off
/// from 1 s to 30 s and retry forever; a refusal sets the write aside.
async fn commit(journal: Arc<Journal>, remote: Arc<Remote>, stats: Arc<Stats>, entry: EntryId) {
    let mut delay = FIRST_RETRY;
    while let Some(job) = journal.next(entry) {
        let outcome = match attempt(&remote, &job).await {
            Ok(version) => {
                bump(&stats.commits, 1);
                delay = FIRST_RETRY;
                Outcome::Committed(version)
            }
            Err(err) if err.is_final() => {
                tracing::warn!(%err, entry = entry.0, "commit rejected; write set aside");
                Outcome::Rejected
            }
            Err(err) => {
                tracing::warn!(%err, entry = entry.0, ?delay, "commit failed; retrying");
                Outcome::Retrying
            }
        };
        let retry = matches!(outcome, Outcome::Retrying);
        journal.finish(&job, outcome);
        if retry {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(LAST_RETRY);
        }
    }
}

/// Chunks the copy, uploads every chunk its base lacks, without asking whether the store
/// already has it (S6), and commits.
async fn attempt(remote: &Remote, job: &Job) -> Result<Version, Error> {
    let base: HashSet<ChunkId> = job.record.base_chunks.iter().copied().collect();
    let copy = File::open(&job.copy)?;
    let (sender, mut chunks) = mpsc::channel(2);
    // Chunking reads and hashes the whole file, so it stays off the async workers.
    let splitter = tokio::task::spawn_blocking(move || {
        for chunk in chunk::split(copy) {
            if sender.blocking_send(chunk).is_err() {
                break;
            }
        }
    });
    let mut refs = Vec::new();
    let mut uploaded = HashSet::new();
    while let Some(chunk) = chunks.recv().await {
        let (chunk, bytes) = chunk?;
        if !base.contains(&chunk.id) && uploaded.insert(chunk.id) {
            remote.put_chunk(chunk.id, Bytes::from(bytes)).await?;
        }
        refs.push(chunk);
    }
    // A splitter that died early would otherwise commit a truncated file.
    splitter.await.map_err(io::Error::other)?;
    remote.commit(&job.record, refs).await
}

impl Journal {
    /// The entry's latest sealed write still to commit. `None` ends its committer.
    pub fn next(&self, entry: EntryId) -> Option<Job> {
        let mut entries = lock(&self.entries);
        let tracked = entries.get_mut(&entry)?;
        if let Some((seal, record)) = &tracked.pending {
            let (seal, record, copy) = (*seal, record.clone(), self.copy_path(entry));
            return Some(Job { seal, record, copy });
        }
        tracked.committing = false;
        self.tidy(&mut entries, entry);
        None
    }

    /// Settles a commit attempt: a committed version becomes the new base, a rejected write
    /// is set aside.
    pub fn finish(&self, job: &Job, outcome: Outcome) {
        let entry = job.record.entry;
        let mut entries = lock(&self.entries);
        let Some(tracked) = entries.get_mut(&entry) else {
            return;
        };
        let committed = matches!(outcome, Outcome::Committed(_));
        tracked.attempts.send_replace((job.seal, committed));
        match outcome {
            Outcome::Retrying => return,
            Outcome::Committed(version) => {
                (tracked.base, tracked.base_chunks) = (Some(version.id), chunk_ids(&version));
            }
            Outcome::Rejected => self.reject(&job.record),
        }
        self.settled.fetch_add(1, Relaxed);
        if tracked
            .pending
            .as_ref()
            .is_some_and(|(seal, _)| *seal == job.seal)
        {
            tracked.pending = None;
            let _ = fs::remove_file(self.record_path(entry));
        }
    }

    /// Moves the record to `rejected/`, with a hard link that keeps its bytes.
    fn reject(&self, record: &Record) {
        let name = format!("{}-{}", record.entry.0, record.request_id);
        let kept = self.dir.join(REJECTED).join(name);
        let result = write_record(&kept.with_extension("json"), record)
            .and_then(|()| fs::hard_link(self.copy_path(record.entry), kept.with_extension("copy")));
        if let Err(err) = result {
            tracing::warn!(%err, "could not keep a rejected write");
        }
    }

    /// The latest pending seal and a feed of commit attempts, for fsync to wait on.
    pub fn pending(&self, entry: EntryId) -> Option<(u64, watch::Receiver<(u64, bool)>)> {
        let entries = lock(&self.entries);
        let tracked = entries.get(&entry)?;
        Some((tracked.pending.as_ref()?.0, tracked.attempts.subscribe()))
    }

    pub fn pending_count(&self) -> usize {
        let entries = lock(&self.entries);
        entries
            .values()
            .filter(|tracked| tracked.pending.is_some())
            .count()
    }
}

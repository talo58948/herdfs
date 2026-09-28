//! The committer: commits sealed journal copies in the background, retrying until the
//! authority answers (C4a, C12). Each entry gets its own task, so one slow file does not
//! hold up the others.

use std::fs::{self, File};
use std::sync::{Arc, atomic::Ordering::Relaxed};
use std::time::{Duration, Instant};
use std::{collections::HashSet, io, path::PathBuf};

use bytes::Bytes;
use herdfs_core::proto::{EntryId, Version};
use herdfs_core::{ChunkId, chunk};
use tokio::sync::{mpsc, watch};

use super::{Journal, REJECTED, Record, write_record};
use crate::stats::{Stats, bump};
use crate::{cache::Cache, error::Error, lock, remote::Remote};

const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(30);

/// The latest sealed write of an entry, to be committed from its sealed file.
pub struct Job {
    pub record: Record,
    pub sealed: PathBuf,
}

pub enum Outcome {
    Committed(Version),
    Rejected,
    Retrying,
}

/// What the commit tasks share.
pub struct Committer {
    pub journal: Arc<Journal>,
    pub remote: Arc<Remote>,
    pub cache: Arc<Cache>,
    pub stats: Arc<Stats>,
}

impl Committer {
    /// Starts a commit task for each entry the journal wakes.
    pub async fn run(self: Arc<Self>, mut woken: mpsc::UnboundedReceiver<EntryId>) {
        while let Some(entry) = woken.recv().await {
            tokio::spawn(self.clone().commit(entry));
        }
    }

    /// Commits the entry's latest sealed write until none is left. Network failures back
    /// off from 1 s to 30 s and retry forever; a refusal sets the write aside.
    async fn commit(self: Arc<Self>, entry: EntryId) {
        let mut delay = FIRST_RETRY;
        while let Some(job) = self.next(entry).await {
            let outcome = match self.attempt(&job).await {
                Ok(version) => {
                    bump(&self.stats.commits, 1);
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
            self.journal.finish(&job, outcome);
            // An unreachable authority is tried again when its offline window ends.
            match self.remote.link.remaining().filter(|_| retry) {
                Some(window) => tokio::time::sleep(window).await,
                None if retry => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(LAST_RETRY);
                }
                None => {}
            }
        }
    }

    /// The entry's next job, once its latest seal has been quiet a while or an fsync asks.
    async fn next(&self, entry: EntryId) -> Option<Job> {
        loop {
            let urged = self.journal.urged.notified();
            let Some(quiet) = self.journal.quiet(entry) else {
                return self.journal.next(entry);
            };
            tokio::select! {
                () = tokio::time::sleep(quiet) => {}
                () = urged => {}
            }
        }
    }

    /// Chunks the sealed copy, caching each chunk and uploading those its base lacks without
    /// asking whether the store already has them (S6), and commits.
    async fn attempt(&self, job: &Job) -> Result<Version, Error> {
        let base = job.record.base.iter().flat_map(|base| &base.chunks);
        let base: HashSet<ChunkId> = base.map(|chunk| chunk.id).collect();
        let sealed = File::open(&job.sealed)?;
        let (sender, mut chunks) = mpsc::channel(2);
        // Chunking reads and hashes the whole file, so it stays off the async workers.
        let splitter = tokio::task::spawn_blocking(move || {
            for chunk in chunk::split(sealed) {
                if sender.blocking_send(chunk).is_err() {
                    break;
                }
            }
        });
        let (mut refs, mut uploaded) = (Vec::new(), HashSet::new());
        while let Some(chunk) = chunks.recv().await {
            let (chunk, bytes) = chunk?;
            // Reading the file back then needs no download of what was just uploaded.
            if !self.cache.contains(&chunk.id)
                && let Err(err) = self.cache.put(&chunk.id, &bytes)
            {
                tracing::warn!(%err, "could not cache a chunk");
            }
            if !base.contains(&chunk.id) && uploaded.insert(chunk.id) {
                self.remote.put_chunk(chunk.id, Bytes::from(bytes)).await?;
            }
            refs.push(chunk);
        }
        // A splitter that died early would otherwise commit a truncated file.
        splitter.await.map_err(io::Error::other)?;
        self.remote.commit(&job.record, refs).await
    }
}

impl Journal {
    /// The entry's latest sealed write still to commit. `None` ends its committer.
    pub fn next(&self, entry: EntryId) -> Option<Job> {
        let mut entries = lock(&self.entries);
        let tracked = entries.get_mut(&entry)?;
        if let Some(record) = &tracked.pending {
            tracked.reading = Some(record.seal);
            let (record, sealed) = (record.clone(), self.sealed_path(entry, record.seal));
            return Some(Job { record, sealed });
        }
        tracked.committing = false;
        self.tidy(&mut entries, entry);
        None
    }

    /// Settles a commit attempt: a committed version becomes the new base, a rejected write
    /// is set aside, and a sealed file that a newer seal replaced goes.
    pub fn finish(&self, job: &Job, outcome: Outcome) {
        let (entry, seal) = (job.record.entry, job.record.seal);
        let mut entries = lock(&self.entries);
        let Some(tracked) = entries.get_mut(&entry) else {
            return;
        };
        tracked.reading = None;
        tracked
            .attempts
            .send_replace((seal, matches!(outcome, Outcome::Committed(_))));
        let settled = match outcome {
            Outcome::Retrying => false,
            Outcome::Committed(version) => {
                tracked.base = Some(version);
                true
            }
            Outcome::Rejected => {
                self.reject(job);
                true
            }
        };
        // Settled, or replaced by a newer seal: nothing reads this sealed copy any more.
        if settled || tracked.sealed.as_ref().is_none_or(|(latest, _)| *latest != seal) {
            let _ = fs::remove_file(&job.sealed);
        }
        if settled && tracked.pending.as_ref().is_some_and(|record| record.seal == seal) {
            (tracked.pending, tracked.sealed) = (None, None);
            let _ = fs::remove_file(self.record_path(entry));
        }
        if settled {
            self.settled.fetch_add(1, Relaxed);
        }
    }

    /// Moves the record to `rejected/`, with a hard link that keeps its bytes.
    fn reject(&self, job: &Job) {
        let record = &job.record;
        let kept = self
            .dir
            .join(REJECTED)
            .join(format!("{}-{}", record.entry.0, record.request_id));
        let result = write_record(&kept.with_extension("json"), record)
            .and_then(|()| fs::hard_link(&job.sealed, kept.with_extension("copy")));
        if let Err(err) = result {
            tracing::warn!(%err, "could not keep a rejected write");
        }
    }

    /// How much longer the committer waits for a newer seal of the entry, if at all.
    pub fn quiet(&self, entry: EntryId) -> Option<Duration> {
        let until = lock(&self.entries).get(&entry)?.quiet_until?;
        Some(until.saturating_duration_since(Instant::now())).filter(|left| !left.is_zero())
    }

    /// Ends the entry's quiet period: an fsync waits for its commit.
    pub fn urge(&self, entry: EntryId) {
        if let Some(tracked) = lock(&self.entries).get_mut(&entry) {
            tracked.quiet_until = None;
        }
        self.urged.notify_waiters();
    }

    /// The latest pending seal and a feed of commit attempts, for fsync to wait on.
    pub fn pending(&self, entry: EntryId) -> Option<(u64, watch::Receiver<(u64, bool)>)> {
        let entries = lock(&self.entries);
        let tracked = entries.get(&entry)?;
        Some((tracked.pending.as_ref()?.seal, tracked.attempts.subscribe()))
    }

    pub fn pending_count(&self) -> usize {
        let entries = lock(&self.entries);
        entries
            .values()
            .filter(|tracked| tracked.pending.is_some())
            .count()
    }
}

//! The committer: commits sealed journal copies in the background, retrying until the
//! authority answers (C4a, C12). Each entry gets its own task, so one slow file does not
//! hold up the others.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::{collections::HashSet, io};

use bytes::Bytes;
use herdfs_core::proto::{EntryId, ErrorCode, Version};
use herdfs_core::{ChunkId, chunk};
use tokio::sync::mpsc;

use super::Record;
use crate::fs::Space;
use crate::stats::bump;
use crate::{Error, Result};

const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(30);
/// The latest sealed write of an entry, to be committed from its sealed file.
pub struct Job {
    pub record: Record,
    pub sealed: PathBuf,
}

pub enum Outcome {
    Committed(Option<Version>),
    Rejected,
    /// The entry is gone, and its write with it: deletion wins, as on a local disk (C10).
    Dropped,
    Retrying,
}

/// What the commit tasks share.
pub struct Committer {
    pub(crate) space: Arc<Space>,
}

impl Committer {
    /// Starts a commit task for each entry the journal wakes.
    pub async fn run(self: Arc<Self>, mut woken: mpsc::UnboundedReceiver<EntryId>) {
        while let Some(entry) = woken.recv().await {
            tokio::spawn(self.clone().commit(entry));
        }
    }

    /// Commits the entry's latest sealed write until none is left. Network failures back
    /// off from 1 s to 30 s, or to the end of an offline window if later, and retry forever;
    /// a refusal sets the write aside.
    async fn commit(self: Arc<Self>, entry: EntryId) {
        let mut delay = FIRST_RETRY;
        while let Some(job) = self.next(entry).await {
            let _ = self.space.commits.subscribe().wait_for(|paused| !*paused).await;
            let attempt = |record| self.attempt(&job.sealed, record);
            let outcome = match commit_dropping_lost_base(&job.record, attempt).await {
                Ok(version) => {
                    bump(&self.space.stats.commits, 1);
                    // Reads go on to the committed version once the journal drops the copy, as
                    // do offline opens and appends (C1, C12).
                    delay = FIRST_RETRY;
                    // A retry may be answered with the head another client committed since.
                    Outcome::Committed(version)
                }
                Err(err) if err.is_not_found() => Outcome::Dropped,
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
            self.finish(&job, outcome);
            if !retry {
                self.space.kernel.invalidate_attrs(entry);
            }
            if retry {
                let window = self.space.remote.link.remaining().unwrap_or_default();
                tokio::time::sleep(window.max(delay)).await;
                delay = (delay * 2).min(LAST_RETRY);
            }
        }
    }

    /// The entry's next job, once its latest seal has been quiet a while or an fsync asks.
    async fn next(&self, entry: EntryId) -> Option<Job> {
        loop {
            let urged = self.space.journal.urged.notified();
            let Some(quiet) = self.space.inodes.local(entry, |local| local.quiet()).flatten() else {
                return self.take(entry);
            };
            let _ = tokio::time::timeout(quiet, urged).await;
        }
    }

    /// Chunks the sealed copy, caching each chunk and uploading those the record's base lacks
    /// without asking whether the store already has them (S6), and commits.
    async fn attempt(&self, sealed: &Path, record: Record) -> Result<Option<Version>> {
        let base = record.base.iter().flat_map(|base| &base.chunks);
        let base: HashSet<ChunkId> = base.map(|chunk| chunk.id).collect();
        let sealed = File::open(sealed)?;
        let (sender, mut chunks) = mpsc::channel(2);
        // Chunking reads and hashes the whole file, so it stays off the async workers.
        let splitter = tokio::task::spawn_blocking(move || {
            let _ = chunk::split(sealed).try_for_each(|chunk| sender.blocking_send(chunk));
        });
        let (mut refs, mut uploaded) = (Vec::new(), HashSet::new());
        let mut upload_error = None;
        while let Some(chunk) = chunks.recv().await {
            let (chunk, bytes) = chunk?;
            // Reading the file back then needs no download of what was just uploaded.
            if !self.space.cache.contains(&chunk.id) {
                self.space.cache.put_best_effort(&chunk.id, &bytes);
            }
            if upload_error.is_none()
                && !base.contains(&chunk.id)
                && uploaded.insert(chunk.id)
                && let Err(error) = self
                    .space
                    .remote
                    .put_chunk(&record.request_id, chunk.id, Bytes::from(bytes))
                    .await
            {
                upload_error = Some(error);
            }
            refs.push(chunk);
        }
        // A splitter that died early would otherwise commit a truncated file.
        splitter.await.map_err(io::Error::other)?;
        // A lost answer may have landed before revocation made a retry's uploads fail.
        let committed = self.space.remote.commit(&record, refs).await;
        match (upload_error, committed) {
            (_, Ok(version)) => Ok(version),
            (Some(upload), Err(Error::Api(commit)))
                if commit.code == ErrorCode::Invalid && !upload.is_final() =>
            {
                Err(upload)
            }
            (_, Err(error)) => Err(error),
        }
    }
}

/// Sends `record` through `attempt`, and once more without its base if the authority no longer
/// retains it (`Conflict` on an unguarded commit), which uploads every chunk: retention never
/// sets a write aside (C4a). A second `Conflict` is final.
pub(super) async fn commit_dropping_lost_base<T, F, Fut>(record: &Record, attempt: F) -> Result<T>
where
    F: Fn(Record) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    match attempt(record.clone()).await {
        Err(Error::Api(error)) if error.code == ErrorCode::Conflict && record.base.is_some() => {
            tracing::info!(
                entry = record.entry.0,
                "base no longer retained; committing without it"
            );
            // Under the same request id, so that a retry after a lost answer commits once.
            let unbased = Record {
                base: None,
                ..record.clone()
            };
            attempt(unbased).await
        }
        attempted => attempted,
    }
}

impl Committer {
    /// The entry's latest sealed write still to commit. `None` ends its committer.
    pub(super) fn take(&self, entry: EntryId) -> Option<Job> {
        let job = self.space.inodes.local(entry, |local| {
            let Some((record, _)) = &local.pending else {
                local.committing = false;
                return None;
            };
            if record.deferred {
                local.committing = false;
                return None;
            }
            let job = Job {
                record: record.clone(),
                sealed: self.space.journal.sealed_path(entry, record.seal),
            };
            (local.reading, local.due) = (Some(job.record.seal), None);
            Some(job)
        });
        job.flatten()
    }

    /// Settles a commit attempt: a committed version becomes the new base, a rejected write is
    /// set aside, and a sealed file that a newer seal replaced goes. A settle drops the copy and
    /// gives the inode the result in one critical section, so no reply pairs the copy's absence
    /// with attributes from before the commit (C1).
    pub(super) fn finish(&self, job: &Job, outcome: Outcome) {
        let (entry, seal) = (job.record.entry, job.record.seal);
        let settled = self.space.switch_version(entry, |state| {
            let local = state.local.as_mut()?;
            local.reading = None;
            let settled = !matches!(outcome, Outcome::Retrying);
            local.settles += u64::from(settled);
            let (committed, version) = match outcome {
                Outcome::Committed(version) => (true, version),
                Outcome::Rejected => {
                    self.space.journal.reject(job);
                    local.refused = local.settles;
                    (false, None)
                }
                Outcome::Dropped | Outcome::Retrying => (false, None),
            };
            local.attempts.send_replace((seal, committed));
            if let Some(version) = &version {
                local.base = Some(version.clone());
            }
            let latest = local.pending.as_ref().map(|(record, _)| record.seal) == Some(seal);
            if settled && latest {
                local.pending = None;
                let _ = fs::remove_file(self.space.journal.record_path(entry));
            }
            // Settled, or replaced by a newer seal: nothing reads this sealed copy any more. It goes
            // after the record, which recovery would otherwise find naming no copy (C4a).
            if settled || !latest {
                let _ = fs::remove_file(&job.sealed);
            }
            let switched = settled && state.settle(version.as_ref(), job.record.mtime_ms);
            Some(switched)
        });
        // This client removed the entry meanwhile, and its local state with it (C10).
        if settled.is_none() {
            let _ = fs::remove_file(&job.sealed);
        }
    }
}

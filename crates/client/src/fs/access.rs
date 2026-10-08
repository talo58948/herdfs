//! The access plan of each read-only access (P1-P3): decided immediately, then begun.

use std::sync::Arc;
use std::time::{Duration, Instant};

use herdfs_core::proto::EntryId;

use super::{Space, handles::Handle};
use crate::lock;
use crate::strategy::{AccessPlan, AccessRecord, Decided, Keep, OpenContext, PlanSource, Upgrade};
use crate::version::OpenVersion;

const REOPENED_WITHIN: Duration = Duration::from_secs(300);
/// Recent opens and access records retained for Jev context and `explain`.
const OPENS: u64 = 4096;

impl Space {
    /// Changes the version an inode follows, then moves retained handles to that version.
    pub(crate) fn switch_version(
        &self,
        entry: EntryId,
        change: impl FnOnce(&mut super::inode::State) -> Option<bool>,
    ) -> Option<bool> {
        let switched = self.inodes.with(entry, change).flatten()?;
        if switched {
            self.follow_retained(entry);
        }
        Some(switched)
    }

    /// Decides the plan without waiting, and records this as the latest access of the inode.
    pub(super) fn start(
        &self,
        file: Option<&Arc<OpenVersion>>,
        pid: u32,
        entry: EntryId,
    ) -> (Arc<AccessRecord>, Option<Upgrade>) {
        let path = self.path(self.ino(entry)).ok().flatten();
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let extension = path
            .as_ref()
            .and_then(|path| std::path::Path::new(path.name()).extension());
        let context = OpenContext {
            size: file.map_or(0, |file| file.version.size),
            extension: extension.map(|extension| extension.to_string_lossy().into()),
            opens: self.opened(entry, comm.trim_end()),
            comm: comm.trim_end().to_owned(),
            coverage: file.map_or(1.0, |file| self.cache.coverage(&file.version.chunks)),
            throughput: self.reader.throughput.get(),
            round_trip: self.remote.round_trip.get().unwrap_or_default(),
        };
        let decision = lock(&self.decider).decide(&context);
        self.stats.decided(decision.decided.plan);
        tracing::debug!(?path, plan = ?decision.decided.plan, source = ?decision.decided.source, ?context, "access plan");
        let record = Arc::new(AccessRecord::new(decision.decided));
        let mut accesses = lock(&self.accesses);
        accesses.insert(entry, 1, record.clone());
        accesses.evict(OPENS, |_| false);
        (record, decision.upgrade)
    }

    fn opened(&self, entry: EntryId, comm: &str) -> u32 {
        let mut opens = lock(&self.opens);
        let key = (entry, comm.to_owned());
        let before = opens
            .get(&key)
            .filter(|(_, last)| last.elapsed() <= REOPENED_WITHIN);
        let count = before.map_or(0, |(count, _)| count);
        opens.insert(key, 1, (count + 1, Instant::now()));
        opens.evict(OPENS, |_| false);
        count
    }

    /// Begins prefetch and retention independently.
    pub(super) fn begin(&self, file: Option<&Arc<OpenVersion>>, handle: &Handle) {
        let Some(access) = &handle.access else {
            return;
        };
        let plan = access.plan();
        if plan.keep == Keep::Yes {
            self.retain(file, handle);
        }
        let Some(file) = file else {
            return;
        };
        let (reader, file, access) = (self.reader.clone(), file.clone(), access.clone());
        tokio::spawn(async move {
            if let Err(err) = reader.prefetch(&file, plan.prefetch, &access.bytes_fetched).await {
                tracing::warn!(%err, entry = file.version.entry.0, "prefetch failed");
            }
        });
    }

    fn retain(&self, file: Option<&Arc<OpenVersion>>, handle: &Handle) {
        let mut pinned = lock(&handle.pinned);
        let chunks = file
            .into_iter()
            .flat_map(|file| &file.version.chunks)
            .map(|chunk| chunk.id)
            .collect::<Vec<_>>();
        self.cache.pin(&chunks);
        if let Some(old) = pinned.replace(chunks) {
            self.cache.unpin(&old);
        }
    }

    pub(super) fn read_ahead(&self, file: Arc<OpenVersion>, after: u64, access: Arc<AccessRecord>) {
        let read_ahead = access.plan().read_ahead;
        let reader = self.reader.clone();
        tokio::spawn(async move {
            if let Err(err) = reader
                .read_ahead(&file, after, read_ahead, &access.bytes_fetched)
                .await
            {
                tracing::warn!(%err, entry = file.version.entry.0, "read-ahead failed");
            }
        });
    }

    fn follow_retained(&self, entry: EntryId) {
        let handles = lock(&self.handles);
        let Some(file) = self.inodes.with(entry, |state| state.followed()).flatten() else {
            return;
        };
        for handle in handles.values().filter(|handle| handle.entry == entry) {
            if lock(&handle.pinned).is_some() {
                self.begin(Some(&file), handle);
            }
        }
    }

    /// Applies confident late settings monotonically while the access is still open.
    pub(super) async fn upgrade(self: Arc<Self>, fh: u64, upgrade: Upgrade) {
        let Ok(answer) = upgrade.await else {
            return;
        };
        // Pinning under the handles lock: release either comes later and unpins, or came first.
        let handles = lock(&self.handles);
        let Some(handle) = handles.get(&fh) else {
            return;
        };
        let Some(access) = &handle.access else {
            return;
        };
        let current = access.decided();
        let plan = current.plan.merged(answer.plan(AccessPlan::NONE));
        if plan == current.plan {
            access.update(Decided {
                answer: Some(answer),
                ..current
            });
            return;
        }
        access.update(Decided {
            plan,
            source: answer.source(PlanSource::Jev),
            answer: Some(answer),
        });
        let file = self.inodes.with(handle.entry, |state| state.followed()).flatten();
        self.begin(file.as_ref(), handle);
    }

    pub(super) fn explain(&self, entry: EntryId) -> Option<Vec<u8>> {
        lock(&self.accesses).get(&entry)?.json().ok()
    }
}

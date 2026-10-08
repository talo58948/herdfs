//! The access plan of each read-only access (P1-P3): decided immediately, then begun.

use std::sync::Arc;
use std::time::{Duration, Instant};

use herdfs_core::proto::EntryId;

use super::{Space, handles::Handle};
use crate::lock;
use crate::reader::{LARGE_CHUNKS, SMALL_CHUNKS};
use crate::strategy::{
    AccessPlan, AccessRecord, Jev, JevAnswer, Keep, OpenContext, PlanSource, ReadAhead, Upgrade,
};
use crate::version::OpenVersion;

const REOPENED_WITHIN: Duration = Duration::from_secs(300);
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
        self.stats.decided(decision.plan);
        tracing::debug!(?path, plan = ?decision.plan, ?decision.source, ?context, "access plan");
        let record = Arc::new(AccessRecord::new(
            decision.plan,
            decision.source,
            decision.confidence,
        ));
        lock(&self.accesses).insert(entry, record.clone());
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
        let (reader, file, bytes) = (self.reader.clone(), file.clone(), access.clone());
        tokio::spawn(async move {
            if let Err(err) = reader.prefetch(&file, plan.prefetch, &bytes.bytes_fetched).await {
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
        let chunks = match access.plan().read_ahead {
            ReadAhead::None => return,
            ReadAhead::Small => SMALL_CHUNKS,
            ReadAhead::Large => LARGE_CHUNKS,
        };
        let reader = self.reader.clone();
        tokio::spawn(async move {
            if let Err(err) = reader
                .read_ahead(&file, after, chunks, &access.bytes_fetched)
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
        let Some(handle) = lock(&self.handles).get(&fh).cloned() else {
            return;
        };
        let Some(access) = &handle.access else {
            return;
        };
        let added = answer.plan(AccessPlan::STREAM, Jev::CONFIDENT);
        let plan = access.plan().merged(added);
        access.update(plan, source(answer), Some(answer.confidence()));
        let file = self.inodes.with(handle.entry, |state| state.followed()).flatten();
        self.begin(file.as_ref(), &handle);
    }

    pub fn plan(&self, entry: EntryId) -> Option<Vec<u8>> {
        lock(&self.accesses).get(&entry).map(|access| access.json())
    }
}

fn source(answer: JevAnswer) -> PlanSource {
    if answer.any_confident(Jev::CONFIDENT) {
        PlanSource::Jev
    } else {
        PlanSource::Rule
    }
}

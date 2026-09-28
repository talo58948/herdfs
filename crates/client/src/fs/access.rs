//! The access strategy of each read-only open (P1): decided at open, then begun.

use std::sync::Arc;
use std::time::{Duration, Instant};

use herdfs_core::SpacePath;
use herdfs_core::proto::EntryId;

use super::{Space, State, handles::Handle};
use crate::lock;
use crate::strategy::{OpenContext, Strategy, Upgrade};
use crate::version::OpenVersion;

/// Opens of a file by one program count as repeated while each comes this soon after the last.
const REOPENED_WITHIN: Duration = Duration::from_secs(300);

/// How many (entry, process name) pairs of recent opens to keep.
const OPENS: u64 = 4096;

impl Space {
    /// Changes the version an inode follows, then moves its Pin handles to that version.
    pub(crate) fn switch_version<T>(
        &self,
        entry: EntryId,
        change: impl FnOnce(&mut State) -> Option<(T, bool)>,
    ) -> Option<T> {
        let (changed, switched) = self.inodes.with(entry, change).flatten()?;
        if switched {
            self.follow_pins(entry);
        }
        Some(changed)
    }

    /// Decides the access strategy for this open of `file` (P1), and begins it.
    pub(super) fn start(
        &self,
        file: &Arc<OpenVersion>,
        path: &SpacePath,
        pid: u32,
        handle: &Handle,
    ) -> Option<Upgrade> {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let extension = std::path::Path::new(path.name()).extension();
        let context = OpenContext {
            size: file.version.size,
            extension: extension.map(|extension| extension.to_string_lossy().into()),
            opens: self.opened(file.version.entry, comm.trim_end()),
            comm: comm.trim_end().to_owned(),
            coverage: self.cache.coverage(&file.version.chunks),
            throughput: self.reader.throughput.get(),
            round_trip: self.remote.round_trip.get().unwrap_or_default(),
        };
        let (strategy, upgrade) = lock(&self.decider).decide(&context);
        self.stats.decided(strategy);
        tracing::debug!(%path, ?strategy, ?context, "access strategy");
        self.begin(strategy, file, handle);
        upgrade
    }

    /// How many times `comm` opened `entry` before, each within [`REOPENED_WITHIN`] of the last,
    /// counting this open for the next.
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

    /// Pins every chunk for Pin, and prefetches them in the background for Fetch and Pin.
    fn begin(&self, strategy: Strategy, file: &Arc<OpenVersion>, handle: &Handle) {
        if strategy == Strategy::Pin {
            let mut pinned = lock(&handle.pinned);
            let chunks = file
                .version
                .chunks
                .iter()
                .map(|chunk| chunk.id)
                .collect::<Vec<_>>();
            self.cache.pin(&chunks);
            if let Some(old) = pinned.replace(chunks) {
                self.cache.unpin(&old);
            }
        }
        if strategy != Strategy::Stream {
            let (reader, file) = (self.reader.clone(), file.clone());
            tokio::spawn(async move {
                if let Err(err) = reader.prefetch(&file).await {
                    tracing::warn!(%err, entry = file.version.entry.0, "prefetch failed");
                }
            });
        }
    }

    /// Moves Pin handles to the version their inode now follows.
    fn follow_pins(&self, entry: EntryId) {
        let handles = lock(&self.handles);
        let Some(file) = self.inodes.with(entry, |state| state.followed()).flatten() else {
            return;
        };
        for handle in handles.values().filter(|handle| handle.entry == entry) {
            if lock(&handle.pinned).is_some() {
                self.begin(Strategy::Pin, &file, handle);
            }
        }
    }

    /// Begins the strategy Jev chose after the open, unless the handle is closed by then.
    pub(super) async fn upgrade(self: Arc<Self>, fh: u64, upgrade: Upgrade) {
        // Pinning under the handles lock: release either comes later and unpins, or came first.
        if let Ok(strategy) = upgrade.await
            && let Some(handle) = lock(&self.handles).get(&fh)
            && let Some(file) = self.inodes.with(handle.entry, |state| state.followed()).flatten()
        {
            self.begin(strategy, &file, handle);
        }
    }
}

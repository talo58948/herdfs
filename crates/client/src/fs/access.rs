//! The access strategy of each read-only open (P1): decided at open, then begun.

use std::sync::Arc;
use std::time::{Duration, Instant};

use herdfs_core::SpacePath;
use herdfs_core::proto::EntryId;

use super::{RECENT_LIMIT, Space, handles::Handle};
use crate::lock;
use crate::reader::OpenVersion;
use crate::strategy::{OpenContext, Strategy, Upgrade};

/// Opens of a file by one program count as repeated while each comes this soon after the last.
const REOPENED_WITHIN: Duration = Duration::from_secs(300);

impl Space {
    /// Decides the access strategy for this open (P1), begins it, and gives the handle its file.
    pub(super) fn start(
        &self,
        file: OpenVersion,
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
        let file = Arc::new(file);
        self.begin(strategy, &file, handle);
        *lock(&handle.content) = Some(file);
        upgrade
    }

    /// How many times `comm` opened `entry` before, each within [`REOPENED_WITHIN`] of the last,
    /// counting this open for the next.
    fn opened(&self, entry: EntryId, comm: &str) -> u32 {
        let mut opens = lock(&self.opens);
        if opens.len() > RECENT_LIMIT {
            opens.clear();
        }
        let (count, last) = opens
            .entry((entry, comm.to_owned()))
            .or_insert((0, Instant::now()));
        if last.elapsed() > REOPENED_WITHIN {
            *count = 0;
        }
        (*count, *last) = (*count + 1, Instant::now());
        *count - 1
    }

    /// Pins every chunk for Pin, and prefetches them in the background for Fetch and Pin.
    fn begin(&self, strategy: Strategy, file: &Arc<OpenVersion>, handle: &Handle) {
        if strategy == Strategy::Pin {
            let mut pinned = lock(&handle.pinned);
            *pinned = file.version.chunks.iter().map(|chunk| chunk.id).collect();
            self.cache.pin(&pinned);
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

    /// Begins the strategy Jev chose after the open, unless the handle is closed by then.
    pub(super) async fn upgrade(self: Arc<Self>, fh: u64, upgrade: Upgrade) {
        // Pinning under the handles lock: release either comes later and unpins, or came first.
        if let Ok(strategy) = upgrade.await
            && let Some(handle) = lock(&self.handles).get(&fh)
            && let Some(file) = lock(&handle.content).clone()
        {
            self.begin(strategy, &file, handle);
        }
    }
}

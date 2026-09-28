//! The sweep: after each collection, deletes the chunk objects that nothing
//! live has referenced for the grace.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use herdfs_core::{ChunkId, proto};
use rusty_s3::{
    S3Action,
    actions::{ListObjectsV2, ListObjectsV2Response},
};

use super::{Server, chunk_key, now_ms, unavailable};
use crate::Policy;

/// Signed store requests are sent at once.
const SIGN_TTL: Duration = Duration::from_secs(60);

/// What one collection did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub pruned: usize,
    /// Chunk lists no retained version used.
    pub dropped: usize,
    /// Chunk objects deleted from the store, and their bytes.
    pub deleted: usize,
    pub bytes: u64,
}

impl Server {
    /// Collects and sweeps every `every` until the process ends.
    pub async fn collect_every(self: Arc<Self>, every: Duration, policy: Policy) {
        let mut ticks = tokio::time::interval(every);
        ticks.tick().await;
        loop {
            ticks.tick().await;
            match self.sweep(now_ms(), &policy).await {
                Ok(Swept {
                    pruned,
                    dropped,
                    deleted,
                    bytes,
                }) => {
                    tracing::info!(pruned, manifests = dropped, deleted, bytes, "collected");
                }
                Err(error) => tracing::warn!(%error, "collection failed"),
            }
        }
    }

    /// Collects, then deletes every chunk object that has been outside the live
    /// set for the grace. A chunk that leaves it did so after the previous
    /// collection found it live, so its grace counts from then (S7), but never
    /// from so early that a URL granted before this collection could outlive
    /// the chunk (S4).
    pub async fn sweep(self: &Arc<Self>, started: i64, policy: &Policy) -> anyhow::Result<Swept> {
        let run_policy = policy.clone();
        let collected = self
            .run(move |authority, _| authority.collect(started, &run_policy).map_err(unavailable))
            .await?;
        let mut stored = Vec::new();
        let mut token = None;
        loop {
            let page = self.list(token.as_deref()).await?;
            for object in page.contents {
                let id = object
                    .key
                    .strip_prefix("chunks/")
                    .and_then(|hex| hex.parse::<ChunkId>().ok());
                stored.extend(id.map(|id| (id, object.size)));
            }
            token = page.next_continuation_token;
            if token.is_none() {
                break;
            }
        }
        let unreferenced: BTreeSet<_> = stored
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| !collected.retained.contains(id))
            .collect();
        let ttl_ms = i64::try_from(self.grant_ttl.as_millis()).unwrap_or(i64::MAX);
        let since_ms = collected.previous_ms.max(started - policy.grace_ms + ttl_ms);
        let since = self
            .run(move |authority, _| {
                authority
                    .unreferenced(&unreferenced, since_ms)
                    .map_err(unavailable)
            })
            .await?;
        let expired = |id: &ChunkId| since.get(id).is_some_and(|at| *at < started - policy.grace_ms);
        let mut doomed: Vec<_> = stored
            .into_iter()
            .filter(|(id, _)| !collected.live.contains(id) && expired(id))
            .collect();
        // Chunks uploaded since the collection began are live again; no upload runs meanwhile.
        let _deleting = self.uploading.write().await;
        let fresh = self
            .run(move |authority, _| authority.uploaded_since(started).map_err(unavailable))
            .await?;
        doomed.retain(|(id, _)| !fresh.contains(id));
        for (id, _) in &doomed {
            self.delete(*id).await?;
        }
        let bytes = doomed.iter().map(|(_, size)| size).sum();
        Ok(Swept {
            pruned: collected.pruned,
            dropped: collected.dropped,
            deleted: doomed.len(),
            bytes,
        })
    }

    async fn list(&self, token: Option<&str>) -> anyhow::Result<ListObjectsV2Response> {
        let mut list = self.bucket.list_objects_v2(Some(&self.credentials));
        list.with_prefix("chunks/");
        if let Some(token) = token {
            list.with_continuation_token(token);
        }
        let reply = self
            .http
            .get(list.sign(SIGN_TTL))
            .send()
            .await?
            .error_for_status()?;
        Ok(ListObjectsV2::parse_response(&reply.text().await?)?)
    }

    async fn delete(&self, id: ChunkId) -> Result<(), proto::Error> {
        let url = self
            .bucket
            .delete_object(Some(&self.credentials), &chunk_key(id))
            .sign(SIGN_TTL);
        let reply = self.http.delete(url).send().await;
        reply
            .and_then(reqwest::Response::error_for_status)
            .map_err(unavailable)?;
        Ok(())
    }
}

//! The sweep: after each collection, deletes the chunk objects that nothing
//! live references and that are older than the grace.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
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
            match self.sweep(&policy).await {
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

    /// Collects, then deletes every chunk object outside the live set whose
    /// last write is older than the grace.
    pub async fn sweep(self: &Arc<Self>, policy: &Policy) -> anyhow::Result<Swept> {
        let started = now_ms();
        let run_policy = policy.clone();
        let collected = self
            .run(move |authority, _| authority.collect(started, &run_policy).map_err(unavailable))
            .await?;
        let mut doomed = Vec::new();
        let mut token = None;
        loop {
            let page = self.list(token.as_deref()).await?;
            for object in page.contents {
                let id = object
                    .key
                    .strip_prefix("chunks/")
                    .and_then(|hex| hex.parse::<ChunkId>().ok());
                let old = older_than_grace(&object.last_modified, started, policy.grace_ms)?;
                doomed.extend(
                    id.filter(|id| old && !collected.live.contains(id))
                        .map(|id| (id, object.size)),
                );
            }
            token = page.next_continuation_token;
            if token.is_none() {
                break;
            }
        }
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

/// Milliseconds since the epoch of an S3 `LastModified` time.
fn modified_ms(time: &str) -> anyhow::Result<i64> {
    let at: jiff::Timestamp = time
        .parse()
        .with_context(|| format!("bad LastModified {time:?}"))?;
    Ok(at.as_millisecond())
}

fn older_than_grace(time: &str, now_ms: i64, grace_ms: i64) -> anyhow::Result<bool> {
    Ok(modified_ms(time)? < now_ms - grace_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grace_boundary_is_kept() {
        // contract: C15, S7
        let at = "1970-01-01T00:00:01Z";
        assert!(!older_than_grace(at, 2_000, 1_000).unwrap());
        assert!(older_than_grace(at, 2_001, 1_000).unwrap());
    }
}

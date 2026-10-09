//! Sealing and synchronizing one entry independently of a kernel filesystem adapter.

use std::io;
use std::sync::Arc;

use herdfs_core::proto::EntryId;

use super::Space;
use crate::Result;

impl Space {
    pub(super) async fn sync_entry(self: &Arc<Self>, entry: EntryId, handle: Option<u64>) -> Result<()> {
        let online = self.remote.link.check().is_ok() && self.remote.link.connected();
        if online && let Some(handle) = handle {
            // NFS is built only on mount_nfs's multi-thread runtime, which supports fsync's
            // block_in_place around the journal's blocking file sync.
            let committed = self.clone().fsync(handle).await;
            return if self.remote.link.connected() {
                committed
            } else {
                Ok(())
            };
        }
        self.seal_entry_async(entry).await?;
        let activated = self
            .inodes
            .local(entry, |local| local.activate(&self.journal, entry));
        activated.unwrap_or(Ok(()))?;
        self.urge(entry);
        if online {
            let committed = self.committed(entry).await;
            if self.remote.link.connected() {
                committed
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    async fn seal_entry_async(self: &Arc<Self>, entry: EntryId) -> Result<()> {
        let space = self.clone();
        tokio::task::spawn_blocking(move || space.seal_entry(entry))
            .await
            .map_err(|error| io::Error::other(error.to_string()))??;
        Ok(())
    }
}

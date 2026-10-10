//! Idle write sealing and removal of unused per-entry flow state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use herdfs_core::proto::EntryId;
use tokio::sync::Mutex as AsyncMutex;

use super::{Flow, IDLE};
use crate::fs::Space;
use crate::fs::nfs::Nfs;
use crate::lock;

impl Nfs {
    pub(in crate::fs::nfs) async fn discard_flow(&self, entry: EntryId) {
        let Some(flow) = self.existing_flow(entry) else {
            return;
        };
        let (epoch, write) = {
            let mut state = flow.lock().await;
            state.serial = state.serial.wrapping_add(1);
            state.epoch_stale = false;
            (state.epoch.take(), state.write.take())
        };
        {
            let mut flows = lock(&self.flows);
            if flows.get(&entry).is_some_and(|found| Arc::ptr_eq(found, &flow)) {
                flows.remove(&entry);
            }
        }
        let handles = epoch.map(|epoch| epoch.handle).into_iter().chain(write);
        for handle in handles {
            if let Err(error) = self.space.clone().release(handle).await {
                tracing::warn!(%error, entry = entry.0, "discarding NFS entry flow failed");
            }
        }
    }
}

pub(super) fn arm_idle(
    space: Arc<Space>,
    flows: Arc<Mutex<HashMap<EntryId, Arc<AsyncMutex<Flow>>>>>,
    entry: EntryId,
    flow: Arc<AsyncMutex<Flow>>,
    serial: u64,
) {
    tokio::spawn(async move {
        loop {
            let deadline = {
                let state = flow.lock().await;
                if state.serial != serial || state.write.is_none() {
                    break;
                }
                if state.in_flight == 0 {
                    state.last_completed.unwrap_or_else(Instant::now) + IDLE
                } else {
                    Instant::now() + IDLE
                }
            };
            tokio::time::sleep_until(deadline.into()).await;
            let mut state = flow.lock().await;
            if state.serial != serial || state.write.is_none() {
                break;
            }
            if state.in_flight != 0
                || state
                    .last_completed
                    .is_some_and(|completed| completed + IDLE > Instant::now())
            {
                continue;
            }
            let handle = state.write.take();
            state.begin();
            drop(state);
            if let Some(handle) = handle {
                if let Err(error) = space.release(handle).await {
                    tracing::warn!(%error, entry = entry.0, "sealing idle NFS write failed");
                }
                flow.lock().await.complete();
            }
            break;
        }
        remove_empty(&flows, entry, &flow).await;
    });
}

pub(super) async fn remove_empty(
    flows: &Mutex<HashMap<EntryId, Arc<AsyncMutex<Flow>>>>,
    entry: EntryId,
    flow: &Arc<AsyncMutex<Flow>>,
) {
    let empty = {
        let state = flow.lock().await;
        state.epoch.is_none() && state.write.is_none() && state.in_flight == 0
    };
    if !empty {
        return;
    }
    let mut flows = lock(flows);
    if flows.get(&entry).is_some_and(|found| Arc::ptr_eq(found, flow)) && Arc::strong_count(flow) == 2 {
        flows.remove(&entry);
    }
}

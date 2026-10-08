//! NFS data-access epochs and whole-file write generations (M2).

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use herdfs_core::proto::EntryId;
use tokio::sync::Mutex as AsyncMutex;

use super::Nfs;
use crate::Result;
use crate::fs::{Attributes, OpenMode, Space, handles::Handle, namespace::AttrChange};
use crate::lock;
use crate::reader::Head;

const IDLE: Duration = Duration::from_secs(1);

struct Epoch {
    head: Head,
    attr: Attributes,
    handle: u64,
}

#[derive(Default)]
pub(super) struct Flow {
    epoch: Option<Epoch>,
    write: Option<u64>,
    last_data: Option<Instant>,
    serial: u64,
}

impl Nfs {
    /// Reads one immutable version until the next access after a one-second data idle (M2, P1).
    pub(super) async fn read_epoch(&self, entry: EntryId, off: u64, count: u32) -> Result<(Vec<u8>, bool)> {
        let flow = self.flow(entry);
        let mut state = flow.lock().await;
        let expired = state.last_data.is_none_or(|last| last.elapsed() >= IDLE);
        if state.write.is_none() && (state.epoch.is_none() || expired) {
            if let Some(epoch) = state.epoch.take() {
                self.space.clone().release(epoch.handle).await?;
            }
            state.epoch = Some(self.start_epoch(entry).await?);
        }
        state.last_data = Some(Instant::now());
        let epoch = state.epoch.as_ref();
        let head = epoch.and_then(|epoch| epoch.head.clone());
        let handle = epoch.map(|epoch| self.space.handle(epoch.handle)).transpose()?;
        drop(state);

        if let Some(copy) = self.local_copy(entry) {
            let size = copy.metadata()?.len();
            return Ok((
                read_copy(&copy, off, count)?,
                off.saturating_add(u64::from(count)) >= size,
            ));
        }
        let Some(file) = head else {
            return Ok((Vec::new(), true));
        };
        let size = file.version.size;
        let handle = handle.ok_or(libc::EBADF)?;
        let bytes = self.space.read_version(Some(file), off, count, &handle).await?;
        Ok((bytes, off.saturating_add(u64::from(count)) >= size))
    }

    pub(super) async fn write_generation(
        &self,
        entry: EntryId,
        off: u64,
        data: &[u8],
        stable: bool,
    ) -> Result<()> {
        let flow = self.flow(entry);
        let mut state = flow.lock().await;
        self.drop_epoch(&mut state).await?;
        let (handle, started) = self.write_handle(entry, &mut state, OpenMode::Write).await?;
        state.last_data = Some(Instant::now());
        if started {
            arm_idle(
                self.space.clone(),
                self.flows.clone(),
                entry,
                flow.clone(),
                state.serial,
            );
        }
        let written = self.space.clone().write(handle, off, data.to_vec()).await;
        if written.is_ok() {
            state.last_data = Some(Instant::now());
        }
        written?;
        if !stable {
            return Ok(());
        }
        state.write = None;
        drop(state);
        let result = self.finish_generation(entry, handle).await;
        remove_empty(&self.flows, entry, &flow).await;
        result
    }

    pub(super) async fn size_generation(
        &self,
        entry: EntryId,
        size: u64,
        mut change: AttrChange,
        initial: Option<u64>,
    ) -> Result<Attributes> {
        let flow = self.flow(entry);
        let mut state = flow.lock().await;
        self.drop_epoch(&mut state).await?;
        let (handle, started) = match (state.write, initial) {
            (Some(handle), _) => (handle, false),
            (None, Some(handle)) => {
                state.write = Some(handle);
                state.serial = state.serial.wrapping_add(1);
                (handle, true)
            }
            (None, None) => {
                let mode = OpenMode::for_set_len(size);
                self.write_handle(entry, &mut state, mode).await?
            }
        };
        state.last_data = Some(Instant::now());
        if started {
            arm_idle(
                self.space.clone(),
                self.flows.clone(),
                entry,
                flow.clone(),
                state.serial,
            );
        }
        change.fh = Some(handle);
        let changed = self.space.clone().setattr(self.space.ino(entry), change).await;
        if changed.is_ok() {
            state.last_data = Some(Instant::now());
        }
        changed
    }

    pub(super) async fn commit_generation(&self, entry: EntryId) -> Result<()> {
        let Some(flow) = self.existing_flow(entry) else {
            return self.space.sync_entry(entry, None).await;
        };
        let mut state = flow.lock().await;
        let handle = state.write.take();
        let epoch = state.epoch.take();
        drop(state);
        if let Some(epoch) = epoch {
            self.space.clone().release(epoch.handle).await?;
        }
        let result = match handle {
            Some(handle) => self.finish_generation(entry, handle).await,
            None => self.space.sync_entry(entry, None).await,
        };
        remove_empty(&self.flows, entry, &flow).await;
        result
    }

    pub(super) async fn flow_attr(&self, entry: EntryId) -> Option<Attributes> {
        let flow = self.existing_flow(entry)?;
        let mut state = flow.lock().await;
        if state.write.is_some() {
            return self.space.known_attr(entry);
        }
        if state.last_data.is_some_and(|last| last.elapsed() < IDLE) {
            return state.epoch.as_ref().map(|epoch| epoch.attr.clone());
        }
        let epoch = state.epoch.take();
        drop(state);
        if let Some(epoch) = epoch
            && let Err(error) = self.space.clone().release(epoch.handle).await
        {
            tracing::warn!(%error, entry = entry.0, "ending NFS read epoch failed");
        }
        remove_empty(&self.flows, entry, &flow).await;
        None
    }

    async fn start_epoch(&self, entry: EntryId) -> Result<Epoch> {
        let head = self.space.head(entry, OpenMode::Read).await?;
        let (access, upgrade) = self.space.start(head.as_ref(), "nfs".to_owned(), entry);
        let handle = Handle::new(entry, false, Some(access), &self.space.inodes);
        self.space.begin(head.as_ref(), &handle);
        let handle = self.space.register(handle);
        if let Some(upgrade) = upgrade {
            tokio::spawn(self.space.clone().upgrade(handle, upgrade));
        }
        let (attr, _) = self.space.clone().getattr(self.space.ino(entry)).await?;
        Ok(Epoch { head, attr, handle })
    }

    async fn write_handle(&self, entry: EntryId, state: &mut Flow, mode: OpenMode) -> Result<(u64, bool)> {
        if let Some(handle) = state.write {
            return Ok((handle, false));
        }
        let handle = self
            .space
            .clone()
            .open(self.space.ino(entry), mode, "nfs")
            .await?;
        state.write = Some(handle);
        state.serial = state.serial.wrapping_add(1);
        Ok((handle, true))
    }

    async fn drop_epoch(&self, state: &mut Flow) -> Result<()> {
        if let Some(epoch) = state.epoch.take() {
            self.space.clone().release(epoch.handle).await?;
        }
        Ok(())
    }

    fn local_copy(&self, entry: EntryId) -> Option<Arc<File>> {
        self.space.inodes.local(entry, |local| local.copy()).flatten()
    }

    async fn finish_generation(&self, entry: EntryId, handle: u64) -> Result<()> {
        let committed = self.space.sync_entry(entry, Some(handle)).await;
        let released = self.space.clone().release(handle).await;
        committed.and(released)
    }
}

fn arm_idle(
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
                state.last_data.unwrap_or_else(Instant::now) + IDLE
            };
            tokio::time::sleep_until(deadline.into()).await;
            let mut state = flow.lock().await;
            if state.serial != serial || state.write.is_none() {
                break;
            }
            if state.last_data.is_some_and(|last| last + IDLE > Instant::now()) {
                continue;
            }
            let handle = state.write.take();
            drop(state);
            if let Some(handle) = handle
                && let Err(error) = space.release(handle).await
            {
                tracing::warn!(%error, entry = entry.0, "sealing idle NFS write failed");
            }
            break;
        }
        remove_empty(&flows, entry, &flow).await;
    });
}

async fn remove_empty(
    flows: &Mutex<HashMap<EntryId, Arc<AsyncMutex<Flow>>>>,
    entry: EntryId,
    flow: &Arc<AsyncMutex<Flow>>,
) {
    let empty = {
        let state = flow.lock().await;
        state.epoch.is_none() && state.write.is_none()
    };
    if !empty {
        return;
    }
    let mut flows = lock(flows);
    if flows.get(&entry).is_some_and(|found| Arc::ptr_eq(found, flow)) && Arc::strong_count(flow) == 2 {
        flows.remove(&entry);
    }
}

fn read_copy(copy: &File, off: u64, count: u32) -> std::io::Result<Vec<u8>> {
    let available = copy.metadata()?.len().saturating_sub(off);
    let mut bytes = vec![0; u64::from(count).min(available) as usize];
    copy.read_exact_at(&mut bytes, off)?;
    Ok(bytes)
}

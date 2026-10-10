//! NFS data-access epochs and whole-file write generations (M2).

use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, Instant};

use herdfs_core::proto::EntryId;

use super::Nfs;
use crate::Result;
use crate::fs::{Attributes, OpenMode, handles::Handle, namespace::AttrChange};
use crate::reader::Head;

mod idle;
use idle::{arm_idle, remove_empty};
mod read;
use read::read_copy;

const IDLE: Duration = Duration::from_secs(1);

struct Epoch {
    head: Head,
    attr: Attributes,
    handle: u64,
    checked: Instant,
}

#[derive(Default)]
pub(super) struct Flow {
    epoch: Option<Epoch>,
    write: Option<u64>,
    in_flight: usize,
    last_completed: Option<Instant>,
    epoch_stale: bool,
    serial: u64,
}

impl Flow {
    fn begin(&mut self) {
        self.in_flight += 1;
    }

    fn complete(&mut self) {
        self.in_flight -= 1;
        self.last_completed = Some(Instant::now());
    }

    fn idle(&self) -> bool {
        self.in_flight == 0
            && self
                .last_completed
                .is_none_or(|completed| completed.elapsed() >= IDLE)
    }
}

impl Nfs {
    /// Reads one immutable version until the next access after a one-second data idle (M2, P1).
    pub(super) async fn read_epoch(&self, entry: EntryId, off: u64, count: u32) -> Result<(Vec<u8>, bool)> {
        let flow = self.flow(entry);
        let mut state = flow.lock().await;
        let copy = self.local_copy(entry);
        if copy.is_none() {
            self.refresh_epoch(entry, &mut state).await?;
            self.revalidate_epoch(entry, &mut state).await?;
        }
        if copy.is_none() && state.write.is_none() && (state.epoch.is_none() || state.idle()) {
            self.end_epoch(&mut state).await?;
            state.epoch = Some(self.start_epoch(entry).await?);
        }
        let epoch = state.epoch.as_ref();
        let head = epoch.and_then(|epoch| epoch.head.clone());
        let handle = epoch.map(|epoch| self.space.handle(epoch.handle)).transpose()?;
        state.begin();
        drop(state);

        let result = async {
            if let Some(copy) = copy {
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
        .await;
        flow.lock().await.complete();
        result
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
        self.expire_epoch(&mut state).await?;
        let (handle, started) = self.write_handle(entry, &mut state, OpenMode::Write).await?;
        if started {
            arm_idle(
                self.space.clone(),
                self.flows.clone(),
                entry,
                flow.clone(),
                state.serial,
            );
        }
        state.begin();
        let written = self.space.clone().write(handle, off, data.to_vec()).await;
        if written.is_ok() {
            state.epoch_stale = state.epoch.is_some();
        }
        if let Err(error) = written {
            state.complete();
            return Err(error);
        }
        if !stable {
            state.complete();
            return Ok(());
        }
        let result = self.finish_generation(entry, handle, &mut state).await;
        state.write = None;
        state.complete();
        drop(state);
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
        self.expire_epoch(&mut state).await?;
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
        if started {
            arm_idle(
                self.space.clone(),
                self.flows.clone(),
                entry,
                flow.clone(),
                state.serial,
            );
        }
        state.begin();
        change.fh = Some(handle);
        let changed = self.space.clone().setattr(self.space.ino(entry), change).await;
        if changed.is_ok() {
            state.epoch_stale = state.epoch.is_some();
        }
        state.complete();
        changed
    }

    pub(super) async fn commit_generation(&self, entry: EntryId) -> Result<()> {
        let Some(flow) = self.existing_flow(entry) else {
            return self.space.sync_entry(entry, None).await;
        };
        let mut state = flow.lock().await;
        state.begin();
        let result = match state.write {
            Some(handle) => self.finish_generation(entry, handle, &mut state).await,
            None => {
                let committed = self.space.sync_entry(entry, None).await;
                let refreshed = if committed.is_ok() {
                    self.refresh_epoch(entry, &mut state).await
                } else {
                    Ok(())
                };
                committed.and(refreshed)
            }
        };
        state.write = None;
        state.complete();
        drop(state);
        remove_empty(&self.flows, entry, &flow).await;
        result
    }

    pub(super) async fn flow_attr(&self, entry: EntryId) -> Option<Attributes> {
        let flow = self.existing_flow(entry)?;
        let mut state = flow.lock().await;
        let known = self.space.known_attr(entry);
        if state.write.is_some() && known.is_some() {
            return known;
        }
        if known.is_some() && !state.idle() {
            return state.epoch.as_ref().map(|epoch| epoch.attr.clone());
        }
        let epoch = state.epoch.take();
        state.epoch_stale = false;
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
        Ok(Epoch {
            head,
            attr,
            handle,
            checked: Instant::now(),
        })
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

    async fn end_epoch(&self, state: &mut Flow) -> Result<()> {
        if let Some(epoch) = state.epoch.take() {
            self.space.clone().release(epoch.handle).await?;
        }
        state.epoch_stale = false;
        Ok(())
    }

    async fn expire_epoch(&self, state: &mut Flow) -> Result<()> {
        if state.idle() {
            self.end_epoch(state).await?;
        }
        Ok(())
    }

    async fn refresh_epoch(&self, entry: EntryId, state: &mut Flow) -> Result<()> {
        if !state.epoch_stale || self.local_copy(entry).is_some() {
            return Ok(());
        }
        let head = self.space.current(entry).await?;
        let attr = self.space.known_attr(entry).ok_or(libc::ENOENT)?;
        if let Some(epoch) = &mut state.epoch {
            (epoch.head, epoch.attr) = (head, attr);
        }
        state.epoch_stale = false;
        Ok(())
    }

    fn local_copy(&self, entry: EntryId) -> Option<Arc<File>> {
        self.space.inodes.local(entry, |local| local.copy()).flatten()
    }

    async fn finish_generation(&self, entry: EntryId, handle: u64, state: &mut Flow) -> Result<()> {
        let committed = self.space.sync_entry(entry, Some(handle)).await;
        let refreshed = if committed.is_ok() {
            self.refresh_epoch(entry, state).await
        } else {
            Ok(())
        };
        let released = self.space.clone().release(handle).await;
        committed.and(refreshed).and(released)
    }
}

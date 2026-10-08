//! NFS data-access epochs and whole-file write generations (M2).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use herdfs_core::proto::EntryId;

use super::Nfs;
use crate::Result;
use crate::fs::{Attributes, OpenMode, Space, handles::Handle};
use crate::reader::Head;

const IDLE: Duration = Duration::from_secs(1);

pub(super) struct Epoch {
    pub(super) head: Head,
    pub(super) attr: Attributes,
    pub(super) handle: u64,
}

#[derive(Default)]
pub(super) struct Flow {
    pub(super) epoch: Option<Epoch>,
    pub(super) write: Option<u64>,
    pub(super) last_data: Option<Instant>,
    write_serial: Arc<AtomicU64>,
}

impl Nfs {
    /// Reads one immutable version until the next access after a one-second data idle (M2, P1).
    pub(super) async fn read_epoch(&self, entry: EntryId, off: u64, count: u32) -> Result<(Vec<u8>, bool)> {
        let flow = self.flow(entry);
        let mut state = flow.lock().await;
        let expired = state.last_data.is_none_or(|last| last.elapsed() >= IDLE);
        if state.epoch.is_none() || expired {
            if let Some(epoch) = state.epoch.take() {
                self.space.clone().release(epoch.handle).await?;
            }
            state.epoch = Some(self.start_epoch(entry).await?);
        }
        state.last_data = Some(Instant::now());
        let head = state.epoch.as_ref().and_then(|epoch| epoch.head.clone());
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
        let bytes = self.space.reader.read(&file, off, u64::from(count)).await?;
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
        if let Some(epoch) = state.epoch.take() {
            self.space.clone().release(epoch.handle).await?;
        }
        let handle = match state.write {
            Some(handle) => handle,
            None => {
                let handle = self
                    .space
                    .clone()
                    .open(self.space.ino(entry), OpenMode::Write, 0)
                    .await?;
                state.write = Some(handle);
                handle
            }
        };
        self.space.clone().write(handle, off, data.to_vec()).await?;
        state.last_data = Some(Instant::now());
        let serial = state.write_serial.fetch_add(1, Ordering::Relaxed) + 1;
        if stable {
            state.write = None;
            drop(state);
            return self.finish_generation(entry, handle).await;
        }
        let serials = state.write_serial.clone();
        drop(state);
        schedule_idle(self.space.clone(), flow, serials, serial);
        Ok(())
    }

    pub(super) async fn commit_generation(&self, entry: EntryId) -> Result<()> {
        let flow = self.flow(entry);
        let mut state = flow.lock().await;
        let handle = state.write.take();
        let epoch = state.epoch.take();
        drop(state);
        if let Some(epoch) = epoch {
            self.space.clone().release(epoch.handle).await?;
        }
        match handle {
            Some(handle) => self.finish_generation(entry, handle).await,
            None => self.space.nfs_commit(entry).await,
        }
    }

    pub(super) async fn epoch_attr(&self, entry: EntryId) -> Option<Attributes> {
        let flow = self.flow(entry);
        let mut state = flow.lock().await;
        if state.last_data.is_some_and(|last| last.elapsed() < IDLE) {
            return state.epoch.as_ref().map(|epoch| epoch.attr.clone());
        }
        let epoch = state.epoch.take();
        drop(state);
        if let Some(epoch) = epoch {
            let _ = self.space.clone().release(epoch.handle).await;
        }
        None
    }

    async fn start_epoch(&self, entry: EntryId) -> Result<Epoch> {
        let head = self.space.head(entry, OpenMode::Read).await?;
        let handle = Handle::new(entry, false, &self.space.inodes);
        let upgrade = self.space.start(head.as_ref(), "nfs".to_owned(), &handle);
        let handle = self.space.register(handle);
        if let Some(upgrade) = upgrade {
            tokio::spawn(self.space.clone().upgrade(handle, upgrade));
        }
        let (attr, _) = self.space.clone().getattr(self.space.ino(entry)).await?;
        Ok(Epoch { head, attr, handle })
    }

    fn local_copy(&self, entry: EntryId) -> Option<Arc<File>> {
        self.space.inodes.local(entry, |local| local.copy()).flatten()
    }

    async fn finish_generation(&self, entry: EntryId, handle: u64) -> Result<()> {
        let committed = self.space.nfs_commit_handle(entry, handle).await;
        let released = self.space.clone().release(handle).await;
        committed.and(released)
    }
}

impl Space {
    async fn nfs_commit_handle(self: &Arc<Self>, entry: EntryId, handle: u64) -> Result<()> {
        if self.remote.link.check().is_ok() {
            return self.clone().fsync(handle).await;
        }
        self.nfs_journal(entry)
    }

    async fn nfs_commit(&self, entry: EntryId) -> Result<()> {
        self.nfs_journal(entry)?;
        if self.remote.link.check().is_err() {
            return Ok(());
        }
        self.committed(entry).await
    }

    fn nfs_journal(&self, entry: EntryId) -> Result<()> {
        tokio::task::block_in_place(|| self.seal_entry(entry))?;
        let activated = self
            .inodes
            .local(entry, |local| local.activate(&self.journal, entry));
        activated.unwrap_or(Ok(()))?;
        self.urge(entry);
        Ok(())
    }
}

fn schedule_idle(
    space: Arc<Space>,
    flow: Arc<tokio::sync::Mutex<Flow>>,
    serials: Arc<AtomicU64>,
    serial: u64,
) {
    tokio::spawn(async move {
        tokio::time::sleep(IDLE).await;
        let mut state = flow.lock().await;
        if serials.load(Ordering::Relaxed) != serial {
            return;
        }
        let handle = state.write.take();
        drop(state);
        if let Some(handle) = handle {
            let _ = space.release(handle).await;
        }
    });
}

fn read_copy(copy: &File, off: u64, count: u32) -> std::io::Result<Vec<u8>> {
    let available = copy.metadata()?.len().saturating_sub(off);
    let mut bytes = vec![0; u64::from(count).min(available) as usize];
    copy.read_exact_at(&mut bytes, off)?;
    Ok(bytes)
}

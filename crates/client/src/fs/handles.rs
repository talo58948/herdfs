//! Open files: reads through the journal copy or the chunk cache, writes into the journal.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{ffi::OsString, fs::File, io, os::unix::fs::FileExt, path::Path};

use fuser::{Errno, FileAttr, INodeNo, OpenAccMode, OpenFlags};
use herdfs_core::proto::{EntryId, Version};
use herdfs_core::{ChunkId, SpacePath};

use super::{PERMISSIONS, RECENT_LIMIT, Space};
use crate::strategy::{OpenContext, Strategy, Upgrade};
use crate::{error::Error, journal::Registration, lock, reader::OpenVersion};

/// Opens of a file by one program count as repeated while each comes this soon after the last.
const REOPENED_WITHIN: Duration = Duration::from_secs(300);

pub struct Handle {
    entry: EntryId,
    /// The version read while the entry has no journal copy; `None` for an empty file.
    content: Option<Arc<OpenVersion>>,
    writable: bool,
    /// Written since the journal copy was last sealed.
    dirty: AtomicBool,
    /// Chunks this handle pinned, which release unpins.
    pinned: Mutex<Vec<ChunkId>>,
    _registration: Registration,
}

impl Handle {
    fn new(entry: EntryId, writable: bool, registration: Registration) -> Self {
        Self {
            entry,
            content: None,
            writable,
            dirty: AtomicBool::new(false),
            pinned: Mutex::default(),
            _registration: registration,
        }
    }
}

impl Space {
    pub async fn open(self: Arc<Self>, ino: INodeNo, flags: OpenFlags, pid: u32) -> Result<u64, Error> {
        let (entry, path) = (self.entry_id(ino), self.path(ino)?);
        let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let mut handle = Handle::new(entry, writable, self.journal.opened(entry));
        let mut upgrade = None;
        if writable {
            let truncate = flags.0 & libc::O_TRUNC != 0;
            self.writable(entry, &path, truncate).await?;
            handle.dirty = truncate.into();
        } else if self.journal.copy(entry).is_none() {
            let head = self.head(entry, &path).await?;
            upgrade = head.and_then(|head| self.start(OpenVersion::new(head), &path, pid, &mut handle));
        }
        let fh = self.register(handle);
        if let Some(upgrade) = upgrade {
            tokio::spawn(self.upgrade(fh, upgrade, path));
        }
        Ok(fh)
    }

    /// Creates an empty file and opens it for writing through a fresh journal copy.
    pub async fn create(
        self: Arc<Self>,
        parent: INodeNo,
        name: OsString,
        mode: u32,
    ) -> Result<(FileAttr, u64), Error> {
        let path = self.child(parent, &name)?;
        let entry = self.remote.create(path, mode & PERMISSIONS).await?;
        let handle = Handle::new(entry.id, true, self.journal.opened(entry.id));
        self.journal
            .install(entry.id, self.journal.scratch(entry.id)?, None)?;
        Ok((self.added(&entry), self.register(handle)))
    }

    pub async fn read(self: Arc<Self>, fh: u64, off: u64, len: u32) -> Result<Vec<u8>, Error> {
        let handle = self.handle(fh)?;
        // A pending local write shadows the remote version for every handle (C1).
        if let Some(copy) = self.journal.copy(handle.entry) {
            return Ok(read_copy(&copy, off, len)?);
        }
        let Some(content) = &handle.content else {
            return Ok(Vec::new());
        };
        // The inode's current path, so a rename on this client keeps reads working; one on
        // another host fails grants until the file is reopened.
        let path = self.path(self.ino(handle.entry))?;
        self.reader.read(content, &path, off, len.into()).await
    }

    pub fn write(&self, fh: u64, off: u64, data: &[u8]) -> Result<u32, Error> {
        let handle = self.handle(fh)?;
        if !handle.writable {
            return Err(Errno::EBADF.into());
        }
        self.journal.write(handle.entry, off, data)?;
        handle.dirty.store(true, Relaxed);
        Ok(data.len() as u32)
    }

    /// Seals the handle's writes. Runs on flush, which close(2) waits for, so a write
    /// survives a crash once close returns (C4a).
    pub async fn flush(self: Arc<Self>, fh: u64) -> Result<(), Error> {
        self.seal(&*self.handle(fh)?)
    }

    pub async fn release(self: Arc<Self>, fh: u64) -> Result<(), Error> {
        let Some(handle) = lock(&self.handles).remove(&fh) else {
            return Ok(());
        };
        self.cache.unpin(&lock(&handle.pinned));
        self.seal(&handle)
    }

    /// Waits for the commit of everything sealed so far. EIO if that attempt fails; the
    /// committer keeps retrying in the background.
    pub async fn fsync(self: Arc<Self>, fh: u64) -> Result<(), Error> {
        let handle = self.handle(fh)?;
        self.seal(&handle)?;
        self.committed(handle.entry).await
    }

    /// Waits for the commit of the entry's pending write, if any. EIO if that attempt fails.
    pub(super) async fn committed(&self, entry: EntryId) -> Result<(), Error> {
        let Some((sealed, mut attempts)) = self.journal.pending(entry) else {
            return Ok(());
        };
        let attempt = attempts.wait_for(|(covered, _)| *covered >= sealed).await;
        let committed = attempt.is_ok_and(|attempt| attempt.1);
        if committed { Ok(()) } else { Err(Errno::EIO.into()) }
    }

    /// Truncates through the journal: on the handle if one is given, else as a short open.
    pub(super) async fn truncate(&self, ino: INodeNo, size: u64, fh: Option<u64>) -> Result<(), Error> {
        let handle = fh
            .map(|fh| self.handle(fh))
            .transpose()?
            .filter(|handle| handle.writable);
        let entry = handle.as_ref().map_or(self.entry_id(ino), |handle| handle.entry);
        let _registration = self.journal.opened(entry);
        self.writable(entry, &self.path(ino)?, size == 0).await?;
        self.journal.set_len(entry, size)?;
        match handle {
            Some(handle) => handle.dirty.store(true, Relaxed),
            None => tokio::task::block_in_place(|| self.journal.seal(entry))?,
        }
        Ok(())
    }

    /// Gives the entry a journal copy, materialized from its head unless truncating (C1).
    async fn writable(&self, entry: EntryId, path: &SpacePath, truncate: bool) -> Result<(), Error> {
        if self.journal.attach(entry, truncate)? {
            return Ok(());
        }
        let head = match self.head(entry, path).await {
            // A truncating write needs no old bytes, and its unguarded commit is right on an
            // unknown base too (C5, C12).
            Err(err) if truncate && err.is_offline() => None,
            head => head?,
        };
        let head = head.map(OpenVersion::new);
        let mut scratch = self.journal.scratch(entry)?;
        if let Some(head) = head.as_ref().filter(|_| !truncate) {
            self.reader.materialize(head, path, &mut scratch.file).await?;
        }
        self.journal
            .install(entry, scratch, head.as_ref().map(|head| &head.version))?;
        // Another open may have installed its copy first; truncate that one instead.
        self.journal.attach(entry, truncate)?;
        Ok(())
    }

    /// The entry's current head. Every open asks, for close-to-open consistency (C2); offline,
    /// the last head seen lets writes and cached reads go on (C12).
    async fn head(&self, entry: EntryId, path: &SpacePath) -> Result<Option<Version>, Error> {
        let (found, head) = match self.remote.open(path.clone()).await {
            Err(err) if err.is_offline() => return lock(&self.heads).get(&entry).cloned().ok_or(err),
            opened => opened?,
        };
        if found.id != entry {
            return Err(Errno::ENOENT.into());
        }
        self.remember(&found);
        let mut heads = lock(&self.heads);
        if heads.len() > RECENT_LIMIT {
            heads.clear();
        }
        let id = |head: &Option<Version>| head.as_ref().map(|version| version.id);
        let seen = heads.insert(entry, head.clone()).map(|seen| id(&seen));
        drop(heads);
        // The kernel may still hold an older version's size and pages, and would serve a
        // longer new one cut short (C3).
        if seen != Some(id(&head))
            && let Some(notifier) = self.notifier.get()
            && let Err(err) = tokio::task::block_in_place(|| notifier.inval_inode(self.ino(entry), 0, 0))
        {
            tracing::debug!(%err, "could not invalidate the kernel's cache");
        }
        Ok(head)
    }

    /// Decides the access strategy for this open (P1), begins it, and gives the handle its file.
    fn start(&self, file: OpenVersion, path: &SpacePath, pid: u32, handle: &mut Handle) -> Option<Upgrade> {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let extension = Path::new(path.name()).extension();
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
        self.begin(strategy, &file, path, handle);
        handle.content = Some(file);
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
    fn begin(&self, strategy: Strategy, file: &Arc<OpenVersion>, path: &SpacePath, handle: &Handle) {
        if strategy == Strategy::Pin {
            let mut pinned = lock(&handle.pinned);
            *pinned = file.version.chunks.iter().map(|chunk| chunk.id).collect();
            self.cache.pin(&pinned);
        }
        if strategy != Strategy::Stream {
            let (reader, file, path) = (self.reader.clone(), file.clone(), path.clone());
            tokio::spawn(async move {
                if let Err(err) = reader.prefetch(&file, &path).await {
                    tracing::warn!(%err, %path, "prefetch failed");
                }
            });
        }
    }

    /// Begins the strategy Jev chose after the open, unless the handle is closed by then.
    async fn upgrade(self: Arc<Self>, fh: u64, upgrade: Upgrade, path: SpacePath) {
        // Pinning under the handles lock: release either comes later and unpins, or came first.
        if let Ok(strategy) = upgrade.await
            && let Some(handle) = lock(&self.handles).get(&fh)
            && let Some(file) = &handle.content
        {
            self.begin(strategy, file, &path, handle);
        }
    }

    /// Persists the handle's writes, if any since the last seal, for the committer.
    fn seal(&self, handle: &Handle) -> Result<(), Error> {
        if !handle.dirty.swap(false, Relaxed) {
            return Ok(());
        }
        // Sealing fsyncs the copy, which may take a while for a large file.
        let sealed = tokio::task::block_in_place(|| self.journal.seal(handle.entry));
        if sealed.is_err() {
            handle.dirty.store(true, Relaxed);
        }
        Ok(sealed?)
    }

    fn register(&self, handle: Handle) -> u64 {
        let fh = self.next_handle.fetch_add(1, Relaxed);
        lock(&self.handles).insert(fh, Arc::new(handle));
        fh
    }

    fn handle(&self, fh: u64) -> Result<Arc<Handle>, Error> {
        Ok(lock(&self.handles).get(&fh).cloned().ok_or(Errno::EBADF)?)
    }
}

fn read_copy(copy: &File, off: u64, len: u32) -> io::Result<Vec<u8>> {
    let available = copy.metadata()?.len().saturating_sub(off);
    let mut bytes = vec![0; u64::from(len).min(available) as usize];
    copy.read_exact_at(&mut bytes, off)?;
    Ok(bytes)
}

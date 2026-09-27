//! Open files: reads through the journal copy or the chunk cache, writes into the journal.

use std::{
    ffi::OsString,
    fs::File,
    io,
    os::unix::fs::FileExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
};

use fuser::{Errno, FileAttr, INodeNo, OpenAccMode, OpenFlags};
use herdfs_core::{
    ChunkId, SpacePath,
    proto::{EntryId, Version},
};

use super::{PERMISSIONS, Space};
use crate::{
    error::Error,
    journal::Registration,
    lock,
    reader::OpenVersion,
    strategy::{OpenContext, Strategy},
};

pub struct Handle {
    entry: EntryId,
    /// The version read while the entry has no journal copy; `None` for an empty file.
    content: Option<Arc<OpenVersion>>,
    writable: bool,
    /// Written since the journal copy was last sealed.
    dirty: AtomicBool,
    pinned: Vec<ChunkId>,
    _registration: Registration,
}

impl Handle {
    fn new(entry: EntryId, writable: bool, registration: Registration) -> Self {
        let (content, dirty, pinned) = (None, AtomicBool::new(false), Vec::new());
        Self {
            entry,
            content,
            writable,
            dirty,
            pinned,
            _registration: registration,
        }
    }
}

impl Space {
    pub async fn open(self: Arc<Self>, ino: INodeNo, flags: OpenFlags, pid: u32) -> Result<u64, Error> {
        let (entry, path) = (self.entry_id(ino), self.path(ino)?);
        let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let mut handle = Handle::new(entry, writable, self.journal.opened(entry));
        if writable {
            let truncate = flags.0 & libc::O_TRUNC != 0;
            self.writable(entry, &path, truncate).await?;
            handle.dirty = truncate.into();
        } else if self.journal.copy(entry).is_none() {
            let content = self
                .head(entry, &path)
                .await?
                .map(|head| OpenVersion::new(path, head));
            handle.content = content.map(|content| self.start(content, pid, &mut handle.pinned));
        }
        Ok(self.register(handle))
    }

    /// Creates an empty file and opens it for writing through a fresh journal copy.
    pub async fn create(
        self: Arc<Self>,
        parent: INodeNo,
        name: OsString,
        mode: u32,
    ) -> Result<(FileAttr, u64), Error> {
        let path = self.child(parent, &name)?;
        let entry = self.remote.create(&path, mode & PERMISSIONS).await?;
        self.changed();
        let handle = Handle::new(entry.id, true, self.journal.opened(entry.id));
        self.journal
            .install(entry.id, self.journal.scratch(entry.id)?, None)?;
        Ok((self.remember(&entry), self.register(handle)))
    }

    pub async fn read(self: Arc<Self>, fh: u64, off: u64, len: u32) -> Result<Vec<u8>, Error> {
        let handle = self.handle(fh)?;
        // A pending local write shadows the remote version for every handle (C1).
        if let Some(copy) = self.journal.copy(handle.entry) {
            return Ok(read_copy(&copy, off, len)?);
        }
        match &handle.content {
            Some(content) => self.reader.read(content, off, len.into()).await,
            None => Ok(Vec::new()),
        }
    }

    pub fn write(&self, fh: u64, off: u64, data: &[u8]) -> Result<u32, Error> {
        let handle = self.handle(fh)?;
        let copy = self.journal.copy(handle.entry).filter(|_| handle.writable);
        copy.ok_or(Errno::EBADF)?.write_all_at(data, off)?;
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
        self.cache.unpin(&handle.pinned);
        self.seal(&handle)
    }

    /// Waits for the commit of everything sealed so far. EIO if that attempt fails; the
    /// committer keeps retrying in the background.
    pub async fn fsync(self: Arc<Self>, fh: u64) -> Result<(), Error> {
        let handle = self.handle(fh)?;
        self.seal(&handle)?;
        let Some((sealed, mut attempts)) = self.journal.pending(handle.entry) else {
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
        self.journal.copy(entry).ok_or(Errno::EIO)?.set_len(size)?;
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
        let head = self
            .head(entry, path)
            .await?
            .map(|head| OpenVersion::new(path.clone(), head));
        let mut scratch = self.journal.scratch(entry)?;
        if let Some(head) = head.as_ref().filter(|_| !truncate) {
            self.reader.materialize(head, &mut scratch.file).await?;
        }
        self.journal
            .install(entry, scratch, head.as_ref().map(|head| &head.version))?;
        // Another open may have installed its copy first; truncate that one instead.
        self.journal.attach(entry, truncate)?;
        Ok(())
    }

    /// The entry's current head. Every open asks, for close-to-open consistency (C2).
    async fn head(&self, entry: EntryId, path: &SpacePath) -> Result<Option<Version>, Error> {
        let (found, head) = self.remote.open(path).await?;
        if found.id != entry {
            return Err(Errno::ENOENT.into());
        }
        self.remember(&found);
        Ok(head)
    }

    /// Decides the access strategy for this open (P1) and starts its prefetch.
    fn start(&self, content: OpenVersion, pid: u32, pinned: &mut Vec<ChunkId>) -> Arc<OpenVersion> {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let extension = Path::new(content.path.name()).extension();
        let context = OpenContext {
            size: content.version.size,
            extension: extension.map(|extension| extension.to_string_lossy().into()),
            comm: comm.trim_end().to_owned(),
            coverage: self.cache.coverage(&content.version.chunks),
        };
        let strategy = lock(&self.decider).decide(&context);
        self.stats.decided(strategy);
        tracing::debug!(
            path = %content.path, ?strategy, size = context.size, extension = ?context.extension,
            comm = context.comm, coverage = context.coverage, "access strategy"
        );
        let content = Arc::new(content);
        if strategy == Strategy::Pin {
            *pinned = content.version.chunks.iter().map(|chunk| chunk.id).collect();
            self.cache.pin(pinned);
        }
        if strategy != Strategy::Stream {
            let (reader, content) = (self.reader.clone(), content.clone());
            tokio::spawn(async move {
                if let Err(err) = reader.prefetch(&content).await {
                    tracing::warn!(%err, path = %content.path, "prefetch failed");
                }
            });
        }
        content
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

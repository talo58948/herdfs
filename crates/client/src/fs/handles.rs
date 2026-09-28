//! Open files: reads through the journal copy or the chunk cache, writes into the journal.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::{ffi::OsString, fs::File, io, os::unix::fs::FileExt};

use fuser::{Errno, FileAttr, INodeNo, OpenAccMode, OpenFlags};
use herdfs_core::proto::{EntryId, ErrorCode, Version};
use herdfs_core::{ChunkId, SpacePath};

use super::{PERMISSIONS, RECENT_LIMIT, Space};
use crate::{error::Error, journal::Registration, lock, reader::OpenVersion};

pub struct Handle {
    entry: EntryId,
    /// The version read while the journal holds no changes: the head at open, or a newer one
    /// this client committed since.
    pub content: Mutex<Option<Arc<OpenVersion>>>,
    writable: bool,
    /// Written since the journal copy was last sealed.
    dirty: AtomicBool,
    /// Chunks this handle pinned, which release unpins.
    pub pinned: Mutex<Vec<ChunkId>>,
    _registration: Registration,
}

impl Handle {
    fn new(entry: EntryId, writable: bool, registration: Registration) -> Self {
        Self {
            entry,
            content: Mutex::default(),
            writable,
            dirty: AtomicBool::new(false),
            pinned: Mutex::default(),
            _registration: registration,
        }
    }
}

impl Space {
    /// Opens the file, and says whether the handle must bypass the kernel's page cache because
    /// another open handle reads a different version.
    pub async fn open(
        self: Arc<Self>,
        ino: INodeNo,
        flags: OpenFlags,
        pid: u32,
    ) -> Result<(u64, bool), Error> {
        let (entry, path) = (self.entry_id(ino), self.path(ino)?);
        let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let truncate = writable && flags.0 & libc::O_TRUNC != 0;
        let handle = Handle::new(entry, writable, self.journal.opened(entry));
        let mut upgrade = None;
        // While this client holds changes, every handle reads them, so no head is needed (C1).
        if self.journal.changes(entry).is_none() {
            let head = self.head(entry, &path, writable, truncate).await?;
            match head.map(OpenVersion::new) {
                Some(head) if !writable => upgrade = self.start(head, &path, pid, &handle),
                head => *lock(&handle.content) = head.map(Arc::new),
            }
        }
        if truncate {
            let base = lock(&handle.content).clone();
            self.prepare(entry, base, true).await?;
            handle.dirty.store(true, Relaxed);
        }
        let (fh, direct) = self.register(handle);
        if let Some(upgrade) = upgrade {
            tokio::spawn(self.upgrade(fh, upgrade));
        }
        Ok((fh, direct))
    }

    /// Creates an empty file and opens it for writing. If another client created the name
    /// first and O_EXCL was not asked for, opens that file instead, as O_CREAT does (C5).
    pub async fn create(
        self: Arc<Self>,
        (parent, name): (INodeNo, OsString),
        mode: u32,
        flags: i32,
        pid: u32,
    ) -> Result<(FileAttr, u64, bool), Error> {
        let path = self.child(parent, &name)?;
        let entry = match self.remote.create(path.clone(), mode & PERMISSIONS).await {
            Err(Error::Api(error)) if error.code == ErrorCode::Exists && flags & libc::O_EXCL == 0 => {
                let entry = self.remote.stat(path).await?;
                self.added(&entry);
                let (fh, direct) = self
                    .clone()
                    .open(self.ino(entry.id), OpenFlags(flags), pid)
                    .await?;
                return Ok((self.attr(&entry), fh, direct));
            }
            created => created?,
        };
        let (fh, direct) = self.register(Handle::new(entry.id, true, self.journal.opened(entry.id)));
        Ok((self.added(&entry), fh, direct))
    }

    pub async fn read(self: Arc<Self>, fh: u64, off: u64, len: u32) -> Result<Vec<u8>, Error> {
        let handle = self.handle(fh)?;
        // Changes pending on this client show through every handle (C1).
        if let Some(copy) = self.journal.changes(handle.entry) {
            return Ok(read_copy(&copy, off, len)?);
        }
        match self.content(&handle) {
            Some(content) => self.reader.read(&content, off, len.into()).await,
            None => Ok(Vec::new()),
        }
    }

    /// Writes into the journal copy, which the first write makes.
    pub async fn write(self: Arc<Self>, fh: u64, off: u64, data: Vec<u8>) -> Result<u32, Error> {
        let handle = self.handle(fh)?;
        if !handle.writable {
            return Err(Errno::EBADF.into());
        }
        self.prepare(handle.entry, self.content(&handle), false).await?;
        self.journal.write(handle.entry, off, &data)?;
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
        let entry = self.entry_id(ino);
        let _registration = self.journal.opened(entry);
        let base = match &handle {
            Some(handle) => self.content(handle),
            None if self.journal.changes(entry).is_some() => None,
            None => self
                .head(entry, &self.path(ino)?, true, size == 0)
                .await?
                .map(OpenVersion::new)
                .map(Arc::new),
        };
        self.prepare(entry, base, size == 0).await?;
        self.journal.set_len(entry, size)?;
        match handle {
            Some(handle) => handle.dirty.store(true, Relaxed),
            None => tokio::task::block_in_place(|| self.journal.seal(entry))?,
        }
        Ok(())
    }

    /// Gives the entry a journal copy to change, emptied if `truncate`: the one holding its
    /// changes, else `base` materialized (C1).
    async fn prepare(
        &self,
        entry: EntryId,
        base: Option<Arc<OpenVersion>>,
        truncate: bool,
    ) -> Result<(), Error> {
        if self.journal.attach(entry, truncate)? {
            return Ok(());
        }
        let mut scratch = self.journal.scratch(entry)?;
        if let Some(base) = base.as_ref().filter(|_| !truncate) {
            self.reader.materialize(base, &mut scratch.file).await?;
        }
        self.journal
            .install(entry, scratch, base.as_ref().map(|base| &base.version))?;
        // Another open may have installed its copy first; truncate that one instead.
        self.journal.attach(entry, truncate)?;
        Ok(())
    }

    /// The version a handle reads without journal changes: its own, or a newer one this
    /// client committed since, whose bytes it holds (C1).
    fn content(&self, handle: &Handle) -> Option<Arc<OpenVersion>> {
        let mut content = lock(&handle.content);
        let known = content.as_ref().map(|content| content.version.id);
        if let Some(own) = self.journal.base_after(handle.entry, known) {
            *content = Some(Arc::new(OpenVersion::new(own)));
        }
        content.clone()
    }

    /// The entry's current head. Every open asks, for close-to-open consistency (C2), and one
    /// for writing needs write access. Offline, the last head seen lets writes and cached reads
    /// go on, and a truncating write needs none, since its unguarded commit is right on an
    /// unknown base too (C5, C12).
    async fn head(
        &self,
        entry: EntryId,
        path: &SpacePath,
        write: bool,
        truncate: bool,
    ) -> Result<Option<Version>, Error> {
        let (found, head, writable) = match self.remote.open(path.clone()).await {
            Err(err) if err.is_offline() => {
                let known = lock(&self.heads).get(&entry).cloned();
                return known.or(truncate.then_some(None)).ok_or(err);
            }
            opened => opened?,
        };
        if found.id != entry {
            return Err(Errno::ENOENT.into());
        }
        if write && !writable {
            return Err(Errno::EACCES.into());
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

    /// Registers the handle, and says whether another open handle on its entry reads a
    /// different version, which the kernel's per-inode page cache would mix up (C3).
    fn register(&self, handle: Handle) -> (u64, bool) {
        let fh = self.next_handle.fetch_add(1, Relaxed);
        let reads = |handle: &Handle| lock(&handle.content).as_ref().map(|content| content.version.id);
        // While this client holds changes, every handle reads them.
        let changes = self.journal.changes(handle.entry).is_some();
        let mut handles = lock(&self.handles);
        let view = reads(&handle);
        let direct = !changes
            && handles
                .values()
                .any(|other| other.entry == handle.entry && reads(other) != view);
        handles.insert(fh, Arc::new(handle));
        (fh, direct)
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

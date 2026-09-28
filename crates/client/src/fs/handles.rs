//! Open files: reads through the journal copy or the entry's current version, writes into the
//! journal.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::{ffi::OsString, fs::File, io, os::unix::fs::FileExt};

use fuser::{Errno, FileAttr, INodeNo, OpenAccMode, OpenFlags};
use herdfs_core::ChunkId;
use herdfs_core::proto::{EntryId, ErrorCode};

use super::{PERMISSIONS, Space};
use tempfile::NamedTempFile;

use crate::journal::{Journal, Registration};
use crate::{Error, Result, lock, reader::Head};

pub struct Handle {
    pub entry: EntryId,
    writable: bool,
    /// Written since the journal copy was last sealed.
    dirty: AtomicBool,
    /// Truncated at open and not sealed since, which only the last close seals.
    truncated: AtomicBool,
    /// The entry's latest seal at this handle's open or last successful fsync.
    synced: AtomicU64,
    /// Chunks this handle pinned, which release unpins.
    pub pinned: Mutex<Vec<ChunkId>>,
    _registration: Registration,
}

impl Handle {
    fn new(entry: EntryId, writable: bool, journal: &Arc<Journal>) -> Self {
        Self {
            entry,
            writable,
            dirty: AtomicBool::new(false),
            truncated: AtomicBool::new(false),
            synced: AtomicU64::new(journal.seals(entry).0),
            pinned: Mutex::default(),
            _registration: journal.opened(entry),
        }
    }
}

impl Space {
    pub async fn open(self: Arc<Self>, ino: INodeNo, flags: OpenFlags, pid: u32) -> Result<u64> {
        let entry = self.entry_id(ino);
        let path = self.located(ino).await?;
        let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let truncate = writable && flags.0 & libc::O_TRUNC != 0;
        let handle = Handle::new(entry, writable, &self.journal);
        let mut upgrade = None;
        // Every open for writing asks for write access, over pending changes too (C1, C4a); one
        // for reading needs no head while this client holds changes, which every handle reads.
        if (writable || self.journal.changes(entry).is_none())
            && let Some(file) = self.head(entry, &path, writable, truncate).await?
            && !writable
        {
            upgrade = self.start(&file, &path, pid, &handle);
        }
        if truncate {
            self.prepare(entry, true).await?;
            handle.truncated.store(true, Relaxed);
        }
        let fh = self.register(handle);
        if let Some(upgrade) = upgrade {
            tokio::spawn(self.upgrade(fh, upgrade));
        }
        Ok(fh)
    }

    /// Creates an empty file and opens it for writing. If another client created the name
    /// first and O_EXCL was not asked for, opens that file instead, as O_CREAT does (C5).
    pub async fn create(
        self: Arc<Self>,
        (parent, name): (INodeNo, OsString),
        mode: u32,
        flags: i32,
        pid: u32,
    ) -> Result<(FileAttr, u64)> {
        let path = self.located_child(parent, &name).await?;
        let entry = match self.remote.create(path.clone(), mode & PERMISSIONS).await {
            Err(Error::Api(error)) if error.code == ErrorCode::Exists && flags & libc::O_EXCL == 0 => {
                let entry = self.remote.stat(path).await?;
                let attr = self.added(&entry);
                let open = self.clone().open(self.ino(entry.id), OpenFlags(flags), pid);
                return Ok((attr, open.await?));
            }
            created => created?,
        };
        self.reader
            .follow(entry.id, (None, true), |entry| self.journal.is_open(entry));
        let fh = self.register(Handle::new(entry.id, true, &self.journal));
        Ok((self.added(&entry), fh))
    }

    pub async fn read(self: Arc<Self>, fh: u64, off: u64, len: u32) -> Result<Vec<u8>> {
        let handle = self.handle(fh)?;
        // Changes pending on this client show through every handle (C1).
        if let Some(copy) = self.journal.changes(handle.entry) {
            return Ok(read_copy(&copy, off, len)?);
        }
        self.usable(handle.entry, |file| self.read_version(file, off, len))
            .await
    }

    async fn read_version(&self, file: Head, off: u64, len: u32) -> Result<Vec<u8>> {
        match file {
            Some(file) => self.reader.read(&file, off, len.into()).await,
            None => Ok(Vec::new()),
        }
    }

    /// Writes into the journal copy, which the first write makes.
    pub async fn write(self: Arc<Self>, fh: u64, off: u64, data: Vec<u8>) -> Result<u32> {
        let handle = self.handle(fh)?;
        if !handle.writable {
            return Err(Errno::EBADF.into());
        }
        self.prepare(handle.entry, false).await?;
        self.journal.write(handle.entry, off, &data)?;
        handle.dirty.store(true, Relaxed);
        Ok(data.len() as u32)
    }

    /// Seals the handle's writes. Runs on flush, which close(2) waits for, so a write
    /// survives a crash once close returns (C4a).
    pub async fn flush(self: Arc<Self>, fh: u64) -> Result<()> {
        self.seal(&*self.handle(fh)?, false)
    }

    pub async fn release(self: Arc<Self>, fh: u64) -> Result<()> {
        let Some(handle) = lock(&self.handles).remove(&fh) else {
            return Ok(());
        };
        self.cache.unpin(&lock(&handle.pinned));
        self.seal(&handle, true)
    }

    /// Waits for the commit of everything sealed so far. EIO if that attempt fails; the
    /// committer keeps retrying in the background.
    pub async fn fsync(self: Arc<Self>, fh: u64) -> Result<()> {
        let handle = self.handle(fh)?;
        self.seal(&handle, true)?;
        self.journal.urge(handle.entry);
        self.committed(handle.entry).await?;
        // A commit refused since this handle's last fsync or open may hold its writes, and a
        // local disk reports a failed writeback to every descriptor open then (C2).
        let (sealed, refused) = self.journal.seals(handle.entry);
        if refused > handle.synced.swap(sealed, Relaxed) {
            return Err(Errno::EIO.into());
        }
        Ok(())
    }

    /// Waits for the commit of the entry's pending write, if any. EIO if that attempt fails.
    pub(super) async fn committed(&self, entry: EntryId) -> Result<()> {
        let Some((sealed, mut attempts)) = self.journal.pending(entry) else {
            return Ok(());
        };
        let attempt = attempts.wait_for(|(covered, _)| *covered >= sealed).await;
        let committed = attempt.is_ok_and(|attempt| attempt.1);
        if committed { Ok(()) } else { Err(Errno::EIO.into()) }
    }

    /// Truncates through the journal: on the handle if one is given, else as a short open,
    /// which needs write access as an open for writing does.
    pub(super) async fn truncate(&self, ino: INodeNo, size: u64, fh: Option<u64>) -> Result<()> {
        let handle = fh.map(|fh| self.handle(fh)).transpose()?;
        let handle = handle.filter(|handle| handle.writable);
        let entry = self.entry_id(ino);
        let _registration = self.journal.opened(entry);
        if handle.is_none() {
            self.head(entry, &self.located(ino).await?, true, size == 0)
                .await?;
        }
        self.prepare(entry, size == 0).await?;
        self.journal.set_len(entry, size)?;
        match handle {
            Some(handle) => handle.dirty.store(true, Relaxed),
            None => tokio::task::block_in_place(|| self.journal.seal(entry))?,
        }
        Ok(())
    }

    /// Gives the entry a journal copy to change, emptied if `truncate`: the one holding its
    /// changes, else its current version materialized (C1).
    async fn prepare(&self, entry: EntryId, truncate: bool) -> Result<()> {
        if self.journal.attach(entry, truncate)? {
            return Ok(());
        }
        // A truncating write needs no old bytes, only the base its commit names.
        let (scratch, base) = match truncate {
            true => (self.journal.scratch(entry)?, self.reader.current(entry).flatten()),
            false => self.usable(entry, |base| self.copy_of(entry, base)).await?,
        };
        self.journal
            .install(entry, scratch, base.as_ref().map(|base| &base.version))?;
        // Another open may have installed its copy first; truncate that one instead.
        self.journal.attach(entry, truncate)?;
        Ok(())
    }

    /// A scratch copy of `base`, to become the entry's working copy.
    async fn copy_of(&self, entry: EntryId, base: Head) -> Result<(NamedTempFile, Head)> {
        let mut scratch = self.journal.scratch(entry)?;
        if let Some(base) = &base {
            self.reader.materialize(base, scratch.as_file_mut()).await?;
        }
        Ok((scratch, base))
    }

    /// Persists the handle's writes, if any since the last seal, for the committer. A truncation
    /// at open alone waits for the last close: a shell's `>` closes its first descriptor before
    /// the command writes through the dup'd one, whose close then seals the data (C3).
    fn seal(&self, handle: &Handle, last: bool) -> Result<()> {
        let truncated = last && handle.truncated.swap(false, Relaxed);
        if !handle.dirty.swap(false, Relaxed) && !truncated {
            return Ok(());
        }
        // Sealing fsyncs the copy, which may take a while for a large file.
        let sealed = tokio::task::block_in_place(|| self.journal.seal(handle.entry));
        Ok(sealed.inspect_err(|_| handle.dirty.store(true, Relaxed))?)
    }

    fn register(&self, handle: Handle) -> u64 {
        let fh = self.next_handle.fetch_add(1, Relaxed);
        lock(&self.handles).insert(fh, Arc::new(handle));
        fh
    }

    fn handle(&self, fh: u64) -> Result<Arc<Handle>> {
        Ok(lock(&self.handles).get(&fh).cloned().ok_or(Errno::EBADF)?)
    }
}

fn read_copy(copy: &File, off: u64, len: u32) -> io::Result<Vec<u8>> {
    let available = copy.metadata()?.len().saturating_sub(off);
    let mut bytes = vec![0; u64::from(len).min(available) as usize];
    copy.read_exact_at(&mut bytes, off)?;
    Ok(bytes)
}

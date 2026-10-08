//! Open files: reads through the journal copy or the entry's current version, writes into the
//! journal.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::{fs::File, io};

use herdfs_core::ChunkId;
use herdfs_core::proto::EntryId;

use tempfile::NamedTempFile;

use super::{InodeNo, Inodes, OpenMode, Space};
use crate::journal::Local;
use crate::strategy::AccessRecord;
use crate::{Result, lock, reader::Head};

pub struct Handle {
    pub entry: EntryId,
    writable: bool,
    /// Written since the journal copy was last sealed.
    dirty: AtomicBool,
    /// Truncated at open and not sealed since, which only the last close seals.
    truncated: AtomicBool,
    /// How many of the entry's commits had settled at this handle's open or last fsync.
    synced: AtomicU64,
    /// Chunks this handle pinned, which release unpins.
    pub pinned: Mutex<Option<Vec<ChunkId>>>,
    /// The read access this handle belongs to, absent for a write-only access.
    pub access: Option<Arc<AccessRecord>>,
    /// End of the preceding read, for per-handle sequential detection.
    sequential_end: Mutex<Option<u64>>,
    /// Keeps the entry's local changes while the handle is open.
    inodes: Arc<Inodes>,
}

impl Handle {
    pub(super) fn new(
        entry: EntryId,
        writable: bool,
        access: Option<Arc<AccessRecord>>,
        inodes: &Arc<Inodes>,
    ) -> Self {
        let settles = inodes.make_local(entry, |local| {
            local.opened();
            local.settles().0
        });
        Self {
            entry,
            writable,
            dirty: AtomicBool::new(false),
            truncated: AtomicBool::new(false),
            synced: AtomicU64::new(settles),
            pinned: Mutex::new(None),
            access,
            sequential_end: Mutex::new(None),
            inodes: inodes.clone(),
        }
    }

    /// True from the second contiguous, nonempty read onward.
    pub(super) fn sequential(&self, off: u64, len: usize) -> bool {
        let mut end = lock(&self.sequential_end);
        let sequential = len != 0 && *end == Some(off);
        *end = Some(off.saturating_add(len as u64));
        sequential
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.inodes.local(self.entry, Local::closed);
    }
}

impl Space {
    pub async fn open(self: Arc<Self>, ino: InodeNo, mode: OpenMode, pid: u32) -> Result<u64> {
        let entry = self.entry_id(ino);
        let (writable, truncate) = (mode.writes(), mode.truncates());
        let (file, access, upgrade) = if writable {
            // Every open for writing asks for write access, over pending changes too (C1, C4a).
            self.head(entry, mode).await?;
            (None, None, None)
        } else {
            let file = match self.changes(entry) {
                Some(_) => None,
                None => self.head(entry, mode).await?,
            };
            let (access, upgrade) = self.start(file.as_ref(), pid, entry);
            (file, Some(access), upgrade)
        };
        let handle = Handle::new(entry, writable, access, &self.inodes);
        self.begin(file.as_ref(), &handle);
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

    /// Writes into the journal copy, which the first write makes.
    pub async fn write(self: Arc<Self>, fh: u64, off: u64, data: Vec<u8>) -> Result<u32> {
        let handle = self.handle(fh)?;
        if !handle.writable {
            return Err(libc::EBADF.into());
        }
        self.prepare(handle.entry, false).await?;
        self.local(handle.entry, |local| {
            local.write(&self.journal, handle.entry, off, &data)
        })?;
        handle.dirty.store(true, Relaxed);
        Ok(data.len() as u32)
    }

    /// Seals the handle's writes. Runs on flush, which close(2) waits for, so a write
    /// survives a crash once close returns (C4a).
    pub async fn flush(self: Arc<Self>, fh: u64) -> Result<()> {
        let handle = self.handle(fh)?;
        if !handle.dirty.load(Relaxed) && handle.truncated.load(Relaxed) {
            return self.defer(handle.entry);
        }
        self.seal(&handle, false)
    }

    pub async fn release(self: Arc<Self>, fh: u64) -> Result<()> {
        let Some(handle) = lock(&self.handles).remove(&fh) else {
            return Ok(());
        };
        if let Some(pinned) = &*lock(&handle.pinned) {
            self.cache.unpin(pinned);
        }
        self.seal(&handle, true)
    }

    /// Seals and waits for the entry's pending write. EIO if that attempt fails; the committer
    /// keeps retrying in the background.
    pub async fn fsync(self: Arc<Self>, fh: u64) -> Result<()> {
        let handle = self.handle(fh)?;
        tokio::task::block_in_place(|| self.seal_entry(handle.entry))?;
        self.local(handle.entry, |local| local.activate(&self.journal, handle.entry))?;
        self.urge(handle.entry);
        self.committed(handle.entry).await?;
        // A commit refused since this handle's last fsync or open may hold its writes, and a
        // local disk reports a failed writeback to every descriptor open then (C2).
        let settles = self.inodes.local(handle.entry, |local| local.settles());
        let (settles, refused) = settles.unwrap_or((0, 0));
        if refused > handle.synced.swap(settles, Relaxed) {
            return Err(libc::EIO.into());
        }
        Ok(())
    }

    /// Waits for the commit of the entry's pending write, if any. EIO if that attempt fails.
    pub(super) async fn committed(&self, entry: EntryId) -> Result<()> {
        let pending = self.inodes.local(entry, |local| local.pending()).flatten();
        let Some((sealed, mut attempts)) = pending else {
            return Ok(());
        };
        if self.commits.is_paused() {
            return Ok(());
        }
        let attempt = attempts.wait_for(|(covered, _)| *covered >= sealed).await;
        let committed = attempt.is_ok_and(|attempt| attempt.1);
        if committed { Ok(()) } else { Err(libc::EIO.into()) }
    }

    /// Truncates through the journal: on the handle if one is given, else as a short open,
    /// which needs write access as an open for writing does.
    pub(super) async fn truncate(&self, ino: InodeNo, size: u64, fh: Option<u64>) -> Result<()> {
        let handle = fh.map(|fh| self.handle(fh)).transpose()?;
        let handle = handle.filter(|handle| handle.writable);
        let entry = self.entry_id(ino);
        // A short open, which keeps the journal copy while it changes.
        let _short = Handle::new(entry, true, None, &self.inodes);
        if handle.is_none() {
            let mode = OpenMode::for_set_len(size);
            self.head(entry, mode).await?;
        }
        self.prepare(entry, size == 0).await?;
        self.local(entry, |local| local.set_len(&self.journal, entry, size))?;
        match handle {
            Some(handle) => handle.dirty.store(true, Relaxed),
            None => tokio::task::block_in_place(|| self.seal_entry(entry))?,
        }
        Ok(())
    }

    /// Gives the entry a journal copy to change, emptied if `truncate`: the one holding its
    /// changes, else its current version materialized (C1).
    async fn prepare(&self, entry: EntryId, truncate: bool) -> Result<()> {
        if self.attach(entry, truncate)? {
            return Ok(());
        }
        // A truncating write needs no old bytes, only the base its commit names.
        let (scratch, base) = if truncate {
            let base = self.inodes.with(entry, |state| state.followed()).flatten();
            (self.journal.scratch(entry)?, base)
        } else {
            self.usable(entry, |base| self.copy_of(entry, base)).await?
        };
        let base = base.as_ref().map(|base| &base.version);
        let install = |local: &mut Local| local.install(&self.journal, entry, scratch, base);
        self.inodes.make_local(entry, install)?;
        // Another open may have installed its copy first; truncate that one instead.
        self.attach(entry, truncate)?;
        Ok(())
    }

    /// Joins the entry's journal copy for writing, emptying it if asked. False if it has none.
    fn attach(&self, entry: EntryId, truncate: bool) -> io::Result<bool> {
        let attached = self.changes(entry).is_some();
        if attached && truncate {
            self.local(entry, |local| local.set_len(&self.journal, entry, 0))?;
        }
        Ok(attached)
    }

    /// The entry's journal copy while it holds changes, which every read sees (C1).
    pub(super) fn changes(&self, entry: EntryId) -> Option<Arc<File>> {
        self.inodes.local(entry, |local| local.copy()).flatten()
    }

    /// Runs `change` on the entry's local changes, which a handle keeps while it is open.
    fn local<T>(&self, entry: EntryId, change: impl FnOnce(&mut Local) -> io::Result<T>) -> io::Result<T> {
        let changed = self.inodes.local(entry, change);
        changed.unwrap_or_else(|| Err(io::Error::other("no journal copy")))
    }

    /// Seals the entry's working copy, if it has one (C3, C4a).
    pub(super) fn seal_entry(&self, entry: EntryId) -> io::Result<()> {
        let seal = |local: &mut Local| local.seal(&self.journal, entry);
        self.inodes.local(entry, seal).unwrap_or(Ok(()))
    }

    /// Ends the entry's quiet period, for an fsync that waits for its commit.
    pub(super) fn urge(&self, entry: EntryId) {
        self.inodes.local(entry, Local::urge);
        self.journal.urged.notify_waiters();
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
        let sealed = tokio::task::block_in_place(|| self.seal_entry(handle.entry));
        sealed.inspect_err(|_| handle.dirty.store(true, Relaxed))?;
        if last {
            self.local(handle.entry, |local| local.activate(&self.journal, handle.entry))?;
        }
        Ok(())
    }

    fn defer(&self, entry: EntryId) -> Result<()> {
        let deferred =
            tokio::task::block_in_place(|| self.local(entry, |local| local.defer(&self.journal, entry)));
        Ok(deferred?)
    }

    pub(super) fn register(&self, handle: Handle) -> u64 {
        let fh = self.next_handle.fetch_add(1, Relaxed);
        lock(&self.handles).insert(fh, Arc::new(handle));
        fh
    }

    pub(super) fn handle(&self, fh: u64) -> Result<Arc<Handle>> {
        Ok(lock(&self.handles).get(&fh).cloned().ok_or(libc::EBADF)?)
    }
}

//! The mounted space: inode numbers, attributes and open handles over the remote namespace.

mod access;
mod answer;
mod fuse;
mod handles;
mod namespace;

use std::sync::{Arc, Mutex, OnceLock, atomic::AtomicU64};
use std::time::{Duration, Instant, UNIX_EPOCH};
use std::{collections::HashMap, ffi::OsStr, io, os::unix::fs::MetadataExt};

pub use fuse::Fuse;
use fuser::{Errno, FileAttr, FileType, INodeNo, Notifier};
use handles::Handle;
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId, Kind};

use crate::{
    Error, Result, cache::Cache, journal::Journal, lock, lru::Lru, reader::Reader, remote::Remote,
    stats::Stats, strategy::Decider,
};

/// How long the kernel and this client may reuse an attribute or listing (T_vis, C2).
const TTL: Duration = Duration::from_secs(1);
/// Only permission bits are stored; the file type comes from the entry kind.
const PERMISSIONS: u32 = 0o7777;
/// The longest name POSIX filesystems take, in bytes.
const MAX_NAME: usize = 255;
/// How many fetched entries to keep, for lookups within TTL and while offline.
const RECENT: u64 = 4096;

/// A directory's entries as readdir reports them.
type Listing = Vec<(INodeNo, FileType, String)>;

pub struct Space {
    remote: Arc<Remote>,
    reader: Arc<Reader>,
    cache: Arc<Cache>,
    journal: Arc<Journal>,
    stats: Arc<Stats>,
    decider: Mutex<Decider>,
    root: EntryId,
    /// The mounting user and group, shown as the owner of every entry.
    owner: (u32, u32),
    /// The path of each inode the kernel holds, with its lookup count, which `forget` lowers,
    /// and when the authority last confirmed the path.
    paths: Mutex<HashMap<INodeNo, (SpacePath, u64, Instant)>>,
    /// Entries fetched from the authority, by path, with when and in which journal
    /// generation (count of settled commits) they were fetched.
    recent: Mutex<Lru<SpacePath, (Instant, u64, Entry)>>,
    /// Opens by entry and process name: how many in a row, and when the last was.
    opens: Mutex<Lru<(EntryId, String), (u32, Instant)>>,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    /// Directory listings taken at opendir, by handle.
    listings: Mutex<HashMap<u64, Listing>>,
    next_handle: AtomicU64,
    /// Tells the kernel to drop what it cached for an inode; set once mounted.
    notifier: OnceLock<Notifier>,
}

impl Space {
    pub fn new(
        remote: Arc<Remote>,
        reader: Arc<Reader>,
        cache: Arc<Cache>,
        journal: Arc<Journal>,
        stats: Arc<Stats>,
        decider: Decider,
        root: EntryId,
    ) -> io::Result<Self> {
        // /proc/self belongs to this process's user and group.
        let process = std::fs::metadata("/proc/self")?;
        let root_path = (INodeNo::ROOT, (SpacePath::root(), 1, Instant::now()));
        Ok(Self {
            remote,
            reader,
            cache,
            journal,
            stats,
            decider: Mutex::new(decider),
            root,
            owner: (process.uid(), process.gid()),
            paths: Mutex::new(HashMap::from([root_path])),
            recent: Mutex::default(),
            opens: Mutex::default(),
            handles: Mutex::default(),
            listings: Mutex::default(),
            next_handle: AtomicU64::new(1),
            notifier: OnceLock::new(),
        })
    }

    /// Inode numbers are entry ids plus one, except that the root is always inode 1.
    fn ino(&self, entry: EntryId) -> INodeNo {
        INodeNo(if entry == self.root { 1 } else { entry.0 + 1 })
    }

    fn entry_id(&self, ino: INodeNo) -> EntryId {
        EntryId(if ino.0 == 1 { self.root.0 } else { ino.0 - 1 })
    }

    fn path(&self, ino: INodeNo) -> Result<SpacePath> {
        let path = lock(&self.paths).get(&ino).map(|(path, ..)| path.clone());
        Ok(path.ok_or(Errno::ENOENT)?)
    }

    pub fn set_notifier(&self, notifier: Notifier) {
        let _ = self.notifier.set(notifier);
    }

    /// The inode's path, which another client may have moved: once older than TTL, asked of
    /// the authority by entry id (C7). Offline, the path held.
    async fn located(&self, ino: INodeNo) -> Result<SpacePath> {
        let known = lock(&self.paths).get(&ino).cloned();
        let (path, _, confirmed) = known.ok_or(Errno::ENOENT)?;
        if ino == INodeNo::ROOT || confirmed.elapsed() < TTL {
            return Ok(path);
        }
        match self.relocate(ino).await {
            Err(err) if err.is_offline() => Ok(path),
            located => Ok(located?.path),
        }
    }

    /// The entry of the inode, wherever it is now.
    async fn relocate(&self, ino: INodeNo) -> Result<Entry> {
        let entry = match self.remote.locate(self.entry_id(ino)).await {
            Err(err) => return Err(self.gone(&self.path(ino)?, err)),
            located => located?,
        };
        self.follow(&entry).await;
        Ok(entry)
    }

    /// The path of `name` in the directory `parent`, wherever that is now.
    async fn located_child(&self, parent: INodeNo, name: &OsStr) -> Result<SpacePath> {
        if name.len() > MAX_NAME {
            return Err(Errno::ENAMETOOLONG.into());
        }
        let (name, parent) = (name.to_str().ok_or(Errno::EINVAL)?, self.located(parent).await?);
        Ok(parent.join(name).map_err(|_| Errno::EINVAL)?)
    }

    /// The entry at `path` and how long the kernel may keep it: for TTL after the fetch, so
    /// opens see commits (C2), else from the authority. Its local changes show through `attr`
    /// (C1). Offline, the last entry seen keeps cached reads and writes working (C12).
    async fn entry(&self, path: &SpacePath) -> Result<(Entry, Duration)> {
        let cached = lock(&self.recent).get(path);
        // A local commit since the fetch has made the entry stale (C1).
        if let Some((fetched, settled, entry)) = &cached
            && *settled == self.journal.settled()
            && let Some(ttl) = TTL.checked_sub(fetched.elapsed())
        {
            return Ok((entry.clone(), ttl));
        }
        let entry = match self.remote.stat(path.clone()).await {
            Err(err) if err.is_offline() => return cached.map(|(.., entry)| (entry, TTL)).ok_or(err),
            stat => stat?,
        };
        self.follow(&entry).await;
        Ok((entry, TTL))
    }

    /// Caches the entry, and the path of its inode if the kernel holds it.
    fn remember(&self, entry: &Entry) {
        if let Some(known) = lock(&self.paths).get_mut(&self.ino(entry.id)) {
            (known.0, known.2) = (entry.path.clone(), Instant::now());
        }
        let mut recent = lock(&self.recent);
        let cached = (Instant::now(), self.journal.settled(), entry.clone());
        recent.insert(entry.path.clone(), 1, cached);
        recent.evict(RECENT, |_| false);
    }

    /// Caches an entry a listing fetched as the next to go unless a lookup uses it, so a big
    /// listing does not push out the entries in use (C12).
    fn listed(&self, entry: &Entry) {
        let mut recent = lock(&self.recent);
        let cached = (Instant::now(), self.journal.settled(), entry.clone());
        recent.insert_cold(entry.path.clone(), 1, cached);
        recent.evict(RECENT, |_| false);
    }

    /// ESTALE for an entry the authority no longer finds where it was looked for. The entry
    /// cached there goes, so the lookup the kernel then makes asks the authority (C7).
    fn gone(&self, path: &SpacePath, err: Error) -> Error {
        if !err.is_not_found() {
            return err;
        }
        lock(&self.recent).remove(path);
        Errno::ESTALE.into()
    }

    /// Counts a reply that hands the kernel a reference to the entry's inode, and returns
    /// its attributes for that reply.
    fn looked_up(&self, entry: &Entry) -> FileAttr {
        let mut paths = lock(&self.paths);
        let known = paths.entry(self.ino(entry.id));
        let known = known.or_insert_with(|| (entry.path.clone(), 0, Instant::now()));
        (known.0, known.1) = (entry.path.clone(), known.1 + 1);
        drop(paths);
        self.attr(entry)
    }

    /// Records an entry this client just made, whose reply hands the kernel its inode.
    fn added(&self, entry: &Entry) -> FileAttr {
        self.stale(&entry.path);
        self.remember(entry);
        self.looked_up(entry)
    }

    /// Drops the kernel's `lookups` references to `ino`, and its path with the last one.
    pub fn forget(&self, ino: INodeNo, lookups: u64) {
        let mut paths = lock(&self.paths);
        let Some((_, count, _)) = paths.get_mut(&ino) else {
            return;
        };
        *count = count.saturating_sub(lookups);
        if *count == 0 && ino != INodeNo::ROOT {
            paths.remove(&ino);
        }
    }

    /// Drops the cached entries at or below `path`, which this client just added, moved or
    /// removed, and its directory's, whose mtime the authority moved with it. The kernel drops
    /// its own attributes of that directory after the change.
    fn stale(&self, path: &SpacePath) {
        let parent = path.parent();
        lock(&self.recent).purge(|known| known.is_within(path) || Some(known) == parent.as_ref());
    }

    /// The entry's attributes, with size and mtime from its journal copy while it has one.
    fn attr(&self, entry: &Entry) -> FileAttr {
        let local = self.journal.changes(entry.id);
        let local = local.and_then(|copy| copy.metadata().ok());
        let size = match (&local, entry.kind) {
            (Some(local), _) => local.len(),
            (None, Kind::Symlink) => entry.target.as_ref().map_or(0, |target| target.len() as u64),
            (None, _) => entry.size,
        };
        let committed = UNIX_EPOCH + Duration::from_millis(entry.mtime_ms.try_into().unwrap_or(0));
        let mtime = local.and_then(|local| local.modified().ok()).unwrap_or(committed);
        let kind = match entry.kind {
            Kind::File => FileType::RegularFile,
            Kind::Dir => FileType::Directory,
            Kind::Symlink => FileType::Symlink,
        };
        FileAttr {
            ino: self.ino(entry.id),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm: (entry.mode & PERMISSIONS) as u16,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.owner.0,
            gid: self.owner.1,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// The counters behind `user.herdfs.stats`.
    pub fn stats(&self) -> Vec<u8> {
        self.stats
            .json(self.journal.pending_count(), self.remote.round_trip.get())
    }
}

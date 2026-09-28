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
use herdfs_core::proto::{Entry, EntryId, Kind, Version};

use crate::{
    cache::Cache, error::Error, journal::Journal, lock, reader::Reader, remote::Remote, stats::Stats,
    strategy::Decider,
};

/// How long the kernel and this client may reuse an attribute or listing (T_vis, C2).
const TTL: Duration = Duration::from_secs(1);
/// Only permission bits are stored; the file type comes from the entry kind.
const PERMISSIONS: u32 = 0o7777;
/// The longest name POSIX filesystems take, in bytes.
const MAX_NAME: usize = 255;
/// Past this many cached entries, expired ones are dropped.
const RECENT_LIMIT: usize = 4096;

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
    /// The path of each inode the kernel holds, with its lookup count, which `forget` lowers.
    paths: Mutex<HashMap<INodeNo, (SpacePath, u64)>>,
    /// Entries fetched from the authority, by path, with when and in which journal
    /// generation (count of settled commits) they were fetched.
    recent: Mutex<HashMap<SpacePath, (Instant, u64, Entry)>>,
    /// The last head `Open` returned for each entry, for opens while offline (C12).
    heads: Mutex<HashMap<EntryId, Option<Version>>>,
    /// Opens by entry and process name: how many in a row, and when the last was.
    opens: Mutex<HashMap<(EntryId, String), (u32, Instant)>>,
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
        cache: Arc<Cache>,
        journal: Arc<Journal>,
        stats: Arc<Stats>,
        decider: Decider,
        root: EntryId,
    ) -> io::Result<Self> {
        // /proc/self belongs to this process's user and group.
        let process = std::fs::metadata("/proc/self")?;
        Ok(Self {
            reader: Arc::new(Reader::new(remote.clone(), cache.clone(), stats.clone())),
            remote,
            cache,
            journal,
            stats,
            decider: Mutex::new(decider),
            root,
            owner: (process.uid(), process.gid()),
            paths: Mutex::new(HashMap::from([(INodeNo::ROOT, (SpacePath::root(), 1))])),
            recent: Mutex::default(),
            heads: Mutex::default(),
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

    fn path(&self, ino: INodeNo) -> Result<SpacePath, Error> {
        let path = lock(&self.paths).get(&ino).map(|(path, _)| path.clone());
        Ok(path.ok_or(Errno::ENOENT)?)
    }

    pub fn set_notifier(&self, notifier: Notifier) {
        let _ = self.notifier.set(notifier);
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Result<SpacePath, Error> {
        if name.len() > MAX_NAME {
            return Err(Errno::ENAMETOOLONG.into());
        }
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        Ok(self.path(parent)?.join(name).map_err(|_| Errno::EINVAL)?)
    }

    /// The entry at `path` and how long the kernel may keep it: from the cache while a local
    /// write is pending or for TTL after the fetch, so opens see commits (C2), else from the
    /// authority.
    async fn entry(&self, path: &SpacePath) -> Result<(Entry, Duration), Error> {
        let cached = lock(&self.recent).get(path).cloned();
        if let Some((fetched, settled, entry)) = &cached {
            // A pending local write shadows the authority, even offline (C1, C12).
            if self.journal.changes(entry.id).is_some() {
                return Ok((entry.clone(), TTL));
            }
            // A local commit since the fetch has made the entry stale (C1).
            let fresh = *settled == self.journal.settled();
            if let Some(ttl) = TTL.checked_sub(fetched.elapsed()).filter(|_| fresh) {
                return Ok((entry.clone(), ttl));
            }
        }
        match self.remote.stat(path.clone()).await {
            Ok(entry) => {
                self.remember(&entry);
                Ok((entry, TTL))
            }
            // Offline, the last entry seen keeps writes to existing files working (C12).
            Err(err) if err.is_offline() => cached.map(|(.., entry)| (entry, TTL)).ok_or(err),
            Err(err) => Err(err),
        }
    }

    /// Caches the entry and updates the path of its inode if the kernel holds it.
    fn remember(&self, entry: &Entry) {
        if let Some(known) = lock(&self.paths).get_mut(&self.ino(entry.id)) {
            known.0 = entry.path.clone();
        }
        let mut recent = lock(&self.recent);
        if recent.len() > RECENT_LIMIT {
            recent.retain(|_, (fetched, ..)| fetched.elapsed() < TTL);
        }
        let cached = (Instant::now(), self.journal.settled(), entry.clone());
        recent.insert(entry.path.clone(), cached);
    }

    /// Counts a reply that hands the kernel a reference to the entry's inode, and returns
    /// its attributes for that reply.
    fn looked_up(&self, entry: &Entry) -> FileAttr {
        let mut paths = lock(&self.paths);
        let known = paths.entry(self.ino(entry.id)).or_default();
        (known.0, known.1) = (entry.path.clone(), known.1 + 1);
        drop(paths);
        self.attr(entry)
    }

    /// Records an entry this client just made, whose reply hands the kernel its inode.
    fn added(&self, entry: &Entry) -> FileAttr {
        self.remember(entry);
        self.looked_up(entry)
    }

    /// Drops the kernel's `lookups` references to `ino`, and its path with the last one.
    pub fn forget(&self, ino: INodeNo, lookups: u64) {
        let mut paths = lock(&self.paths);
        let Some((_, count)) = paths.get_mut(&ino) else {
            return;
        };
        *count = count.saturating_sub(lookups);
        if *count == 0 && ino != INodeNo::ROOT {
            paths.remove(&ino);
        }
    }

    /// Drops the cached entries at or below `path`, which this client just moved or removed.
    fn stale(&self, path: &SpacePath) {
        lock(&self.recent).retain(|known, _| !known.is_within(path));
    }

    /// The entry's attributes, with size and mtime from its journal copy while it has one.
    fn attr(&self, entry: &Entry) -> FileAttr {
        let local = self
            .journal
            .changes(entry.id)
            .and_then(|copy| copy.metadata().ok());
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

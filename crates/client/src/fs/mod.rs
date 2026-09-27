//! The mounted space: inode numbers, attributes and open handles over the remote namespace.

mod fuse;
mod handles;
mod namespace;

use std::{
    collections::HashMap,
    ffi::OsStr,
    io,
    os::unix::fs::MetadataExt,
    sync::{Arc, Mutex, atomic::AtomicU64},
    time::{Duration, Instant, UNIX_EPOCH},
};

pub use fuse::Fuse;
use fuser::{Errno, FileAttr, FileType, INodeNo};
use handles::Handle;
use herdfs_core::{
    SpacePath,
    proto::{Entry, EntryId, Kind},
};

use crate::{
    cache::Cache, error::Error, journal::Journal, lock, reader::Reader, remote::Remote, stats::Stats,
    strategy::Decider,
};

/// How long the kernel and this client may reuse an attribute or listing (T_vis, C2).
const TTL: Duration = Duration::from_secs(1);
/// Only permission bits are stored; the file type comes from the entry kind.
const PERMISSIONS: u32 = 0o7777;
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
    paths: Mutex<HashMap<INodeNo, SpacePath>>,
    /// Entries fetched from the authority, by path, with when and in which journal
    /// generation (count of settled commits) they were fetched.
    recent: Mutex<HashMap<SpacePath, (Instant, u64, Entry)>>,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    /// Directory listings taken at opendir, by handle.
    listings: Mutex<HashMap<u64, Listing>>,
    next_handle: AtomicU64,
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
            paths: Mutex::new(HashMap::from([(INodeNo::ROOT, SpacePath::root())])),
            recent: Mutex::default(),
            handles: Mutex::default(),
            listings: Mutex::default(),
            next_handle: AtomicU64::new(1),
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
        Ok(lock(&self.paths).get(&ino).cloned().ok_or(Errno::ENOENT)?)
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Result<SpacePath, Error> {
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        Ok(self.path(parent)?.join(name).map_err(|_| Errno::EINVAL)?)
    }

    /// The entry at `path` and how long the kernel may keep it: from the cache while a local
    /// write is pending or for TTL after the fetch, so opens see commits (C2), else from the
    /// authority.
    async fn entry(&self, path: &SpacePath) -> Result<(Entry, Duration), Error> {
        let cached = lock(&self.recent).get(path).cloned();
        if let Some((fetched, settled, entry)) = cached {
            // A pending local write shadows the authority, even offline (C1, C12).
            if self.journal.copy(entry.id).is_some() {
                return Ok((entry, TTL));
            }
            // A local commit since the fetch has made the entry stale (C1).
            let fresh = settled == self.journal.settled();
            if let Some(ttl) = TTL.checked_sub(fetched.elapsed()).filter(|_| fresh) {
                return Ok((entry, ttl));
            }
        }
        let entry = self.remote.stat(path).await?;
        self.remember(&entry);
        Ok((entry, TTL))
    }

    /// Maps the entry's inode to its path, caches it, and returns its attributes.
    fn remember(&self, entry: &Entry) -> FileAttr {
        lock(&self.paths).insert(self.ino(entry.id), entry.path.clone());
        let mut recent = lock(&self.recent);
        if recent.len() > RECENT_LIMIT {
            recent.retain(|_, (fetched, ..)| fetched.elapsed() < TTL);
        }
        let cached = (Instant::now(), self.journal.settled(), entry.clone());
        recent.insert(entry.path.clone(), cached);
        drop(recent);
        self.attr(entry)
    }

    /// Drops cached entries after a local change.
    fn changed(&self) {
        lock(&self.recent).clear();
    }

    /// The entry's attributes, with size and mtime from its journal copy while it has one.
    fn attr(&self, entry: &Entry) -> FileAttr {
        let local = self.journal.copy(entry.id).and_then(|copy| copy.metadata().ok());
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
        self.stats.json(self.journal.pending_count())
    }
}

//! The mounted space: inode numbers, attributes and open handles over the remote namespace.

mod access;
mod answer;
mod fuse;
mod handles;
mod namespace;
mod paths;

use std::sync::{Arc, Mutex, OnceLock, atomic::AtomicU64};
use std::time::{Duration, Instant};
use std::{collections::HashMap, fs::File, io, os::unix::fs::MetadataExt};

pub use fuse::Fuse;
use fuser::{Errno, FileAttr, FileType, INodeNo, Notifier};
use handles::Handle;
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId, Kind};

use crate::{
    Result, cache::Cache, journal::Journal, lock, lru::Lru, reader::Reader, remote::Remote, stats::Stats,
    strategy::Decider,
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
/// An inode's path, its lookup count, which `forget` lowers, and when the authority last
/// confirmed the path: `None` once in doubt.
type Known = (SpacePath, u64, Option<Instant>);
/// When a call to the authority was sent, and the count of settled commits then: what it
/// answers is current as of both.
type Asked = (Instant, u64);

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
    /// Each inode the kernel holds.
    paths: Mutex<HashMap<INodeNo, Known>>,
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
        let root_path = (INodeNo::ROOT, (SpacePath::root(), 1, None));
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

    /// Drops the kernel's pages of the entry, which a newer version replaced, before the reply
    /// that switched it, so no later read sees the old one (C3). The kernel waits for the pages
    /// a read in progress holds, which other tasks answer; reads and writes, which hold pages
    /// themselves, never switch a version known before.
    async fn invalidate(&self, entry: EntryId) {
        if let Some(notifier) = self.notifier.get().cloned() {
            let ino = self.ino(entry);
            let _ = tokio::task::spawn_blocking(move || notifier.inval_inode(ino, 0, 0)).await;
        }
    }

    /// The entry's attributes, with size and mtime from its journal copy while it has one.
    /// When a call that a reply depends on is sent: before it, so that a commit settling during
    /// the call makes its answer stale (C1).
    fn asked(&self) -> Asked {
        (Instant::now(), self.journal.settled())
    }

    fn attr(&self, entry: &Entry, local: Option<&File>) -> FileAttr {
        let local = local.and_then(|copy| copy.metadata().ok());
        let size = match (&local, entry.kind) {
            (Some(local), _) => local.len(),
            (None, Kind::Symlink) => entry.target.as_ref().map_or(0, |target| target.len() as u64),
            (None, _) => entry.size,
        };
        // The journal copy's time as its commit will keep it, to the millisecond.
        let written = local.and_then(|local| local.modified().ok()).map(crate::unix_ms);
        let mtime = crate::from_unix_ms(written.unwrap_or(entry.mtime_ms));
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

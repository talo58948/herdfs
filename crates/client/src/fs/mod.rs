//! The mounted space: inode numbers, attributes and open handles over the remote namespace.

mod access;
mod answer;
mod fuse;
mod handles;
mod inodes;
mod namespace;
mod paths;
mod versions;

use std::sync::{Arc, Mutex, OnceLock, atomic::AtomicU64};
use std::time::{Duration, Instant};
use std::{collections::HashMap, fs::File, io, os::unix::fs::MetadataExt};

pub use fuse::Fuse;
use fuser::{FileAttr, FileType, INodeNo, Notifier};
use handles::Handle;
use inodes::Inodes;
use herdfs_core::proto::{Entry, EntryId, Kind};
use tokio::task::JoinHandle;

use crate::{
    cache::Cache, journal::Journal, lru::Lru, reader::Reader, remote::Remote, stats::Stats, strategy::Decider,
};

/// How long the kernel and this client may reuse an attribute or listing (T_vis, C2).
const TTL: Duration = Duration::from_secs(1);
/// Only permission bits are stored; the file type comes from the entry kind.
const PERMISSIONS: u32 = 0o7777;
/// The longest name POSIX filesystems take, in bytes.
const MAX_NAME: usize = 255;

/// A directory's entries as readdir reports them.
type Listing = Vec<(INodeNo, FileType, String)>;
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
    inodes: Inodes,
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
        Ok(Self {
            remote,
            reader,
            cache,
            journal,
            stats,
            decider: Mutex::new(decider),
            root,
            owner: (process.uid(), process.gid()),
            inodes: Inodes::default(),
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

    pub fn set_notifier(&self, notifier: Notifier) {
        let _ = self.notifier.set(notifier);
    }

    /// Drops the kernel's pages and attributes of the entry, whose bytes a settled commit may
    /// have made other than those its journal copy served (C3), without waiting for it.
    pub fn forget_pages(&self, entry: EntryId) {
        let _ = self.invalidate(entry);
    }

    /// Starts dropping the kernel's pages of the entry, which a newer version replaced. A reply
    /// that switched the version waits for it, so no later read sees the old one (C3): the kernel
    /// waits for the pages a read in progress holds, which other tasks answer. A read or write,
    /// which holds pages itself, lets it run.
    fn invalidate(&self, entry: EntryId) -> Option<JoinHandle<io::Result<()>>> {
        let (notifier, ino) = (self.notifier.get().cloned()?, self.ino(entry));
        Some(tokio::task::spawn_blocking(move || {
            notifier.inval_inode(ino, 0, 0)
        }))
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

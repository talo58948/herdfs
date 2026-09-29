//! The mounted space: inode numbers, attributes and open handles over the remote namespace.

mod access;
mod answer;
mod ask;
mod fuse;
mod handles;
mod inode;
mod inodes;
mod kernel;
mod namespace;

use std::sync::{Arc, Mutex, atomic::AtomicU64};
use std::time::{Duration, Instant};
use std::{collections::HashMap, fs::File, io, os::unix::fs::MetadataExt};

pub use fuse::Fuse;
use fuser::{FileAttr, FileType, INodeNo, Notifier, OpenAccMode, OpenFlags};
use handles::Handle;
pub use inodes::Inodes;
pub use kernel::Kernel;
use herdfs_core::proto::{Entry, EntryId, Kind};

use crate::{
    cache::Cache,
    journal::{Journal, Local},
    lru::Lru,
    reader::Reader,
    remote::Remote,
    stats::Stats,
    strategy::Decider,
};

/// How long the kernel and this client may reuse an attribute or listing (T_vis, C2).
const TTL: Duration = Duration::from_secs(1);
/// Only permission bits are stored; the file type comes from the entry kind.
const PERMISSIONS: u32 = 0o7777;
/// The longest name POSIX filesystems take, in bytes.
const MAX_NAME: usize = 255;

/// A directory's entries as readdir reports them.
type Listing = Vec<(INodeNo, FileType, String)>;

#[derive(Clone, Copy)]
enum OpenMode {
    Read,
    Write,
    Truncate,
}

impl OpenMode {
    fn for_open(flags: OpenFlags) -> Self {
        match (flags.acc_mode(), flags.0 & libc::O_TRUNC != 0) {
            (OpenAccMode::O_RDONLY, _) => Self::Read,
            (_, true) => Self::Truncate,
            _ => Self::Write,
        }
    }

    fn for_set_len(size: u64) -> Self {
        if size == 0 { Self::Truncate } else { Self::Write }
    }

    fn writes(self) -> bool {
        !matches!(self, Self::Read)
    }

    fn truncates(self) -> bool {
        matches!(self, Self::Truncate)
    }
}

pub struct Space {
    pub(crate) remote: Arc<Remote>,
    reader: Arc<Reader>,
    pub(crate) cache: Arc<Cache>,
    pub(crate) journal: Arc<Journal>,
    pub(crate) stats: Arc<Stats>,
    decider: Mutex<Decider>,
    pub(crate) kernel: Arc<Kernel>,
    /// The mounting user and group, shown as the owner of every entry.
    owner: (u32, u32),
    pub(crate) inodes: Arc<Inodes>,
    /// Opens by entry and process name: how many in a row, and when the last was.
    opens: Mutex<Lru<(EntryId, String), (u32, Instant)>>,
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
        kernel: Arc<Kernel>,
        inodes: Arc<Inodes>,
    ) -> io::Result<Self> {
        // /proc/self belongs to this process's user and group.
        let process = std::fs::metadata("/proc/self")?;
        let reader = Arc::new(Reader::new(remote.clone(), cache.clone(), stats.clone()));
        Ok(Self {
            remote,
            reader,
            cache,
            journal,
            stats,
            decider: Mutex::new(decider),
            kernel,
            owner: (process.uid(), process.gid()),
            inodes,
            opens: Mutex::default(),
            handles: Mutex::default(),
            listings: Mutex::default(),
            next_handle: AtomicU64::new(1),
        })
    }

    /// Inode numbers are entry ids plus one, except that the root is always inode 1.
    fn ino(&self, entry: EntryId) -> INodeNo {
        self.kernel.ino(entry)
    }

    fn entry_id(&self, ino: INodeNo) -> EntryId {
        self.kernel.entry(ino)
    }

    pub fn mounted(&self, notifier: Notifier) {
        self.kernel.mounted(notifier);
    }

    /// Counts a reply that hands the kernel a reference to the entry's inode, and returns its
    /// attributes for that reply.
    fn looked_up(&self, entry: &Entry, asked: Instant, local: Option<&File>) -> FileAttr {
        self.inodes.looked_up(entry, asked);
        self.attr(entry, local)
    }

    /// Records an entry this client just made, whose reply hands the kernel its inode.
    fn added(&self, entry: &Entry) -> FileAttr {
        self.inodes.stale(&entry.path);
        let (entry, local) = self.inodes.view(entry.clone());
        self.looked_up(&entry, Instant::now(), local.as_deref())
    }

    /// Drops the kernel's `lookups` references to `ino`.
    pub fn forget(&self, ino: INodeNo, lookups: u64) {
        self.inodes.forget(self.entry_id(ino), lookups);
    }

    /// The entry's attributes, with size and mtime from its journal copy while it has one.
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
            .json(self.inodes.count(Local::is_pending), self.remote.round_trip.get())
    }
}

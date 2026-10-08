//! The mounted space: inode numbers, attributes and open handles over the remote namespace.

mod access;
#[cfg(target_os = "linux")]
mod answer;
mod ask;
#[cfg(target_os = "linux")]
mod fuse;
mod handles;
mod inode;
mod inodes;
mod kernel;
mod namespace;
mod read;
mod xattr;

use std::sync::{Arc, Mutex, atomic::AtomicU64};
use std::time::{Duration, Instant, SystemTime};
use std::{collections::HashMap, ffi::OsString, fs::File};

#[cfg(target_os = "linux")]
pub use fuse::Fuse;
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
    strategy::{AccessRecord, Decider},
};

/// How long the kernel and this client may reuse an attribute or listing (T_vis, C2).
const TTL: Duration = Duration::from_secs(1);
/// Only permission bits are stored; the file type comes from the entry kind.
const PERMISSIONS: u32 = 0o7777;
/// The longest name POSIX filesystems take, in bytes.
const MAX_NAME: usize = 255;

/// A directory's entries as readdir reports them.
type Listing = Vec<(InodeNo, FileKind, String)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InodeNo(u64);

impl InodeNo {
    const ROOT: Self = Self(1);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileKind {
    File,
    Directory,
    Symlink,
}

pub(crate) struct Attributes {
    ino: InodeNo,
    size: u64,
    blocks: u64,
    time: SystemTime,
    kind: FileKind,
    perm: u16,
    nlink: u32,
    uid: u32,
    gid: u32,
    blksize: u32,
}

pub(crate) struct DirectoryEntry {
    ino: InodeNo,
    kind: FileKind,
    name: String,
    next: u64,
}

pub(crate) enum Invalidation {
    Attributes(InodeNo),
    Entry {
        parent: InodeNo,
        child: InodeNo,
        name: OsString,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum OpenMode {
    Read,
    Write,
    Truncate,
}

impl OpenMode {
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
    /// Latest access record by entry, retained after its handle closes for `explain`.
    accesses: Mutex<Lru<EntryId, Arc<AccessRecord>>>,
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
    ) -> Self {
        let reader = Arc::new(Reader::new(remote.clone(), cache.clone(), stats.clone()));
        Self {
            remote,
            reader,
            cache,
            journal,
            stats,
            decider: Mutex::new(decider),
            accesses: Mutex::default(),
            kernel,
            owner: (
                rustix::process::geteuid().as_raw(),
                rustix::process::getegid().as_raw(),
            ),
            inodes,
            opens: Mutex::default(),
            handles: Mutex::default(),
            listings: Mutex::default(),
            next_handle: AtomicU64::new(1),
        }
    }

    /// Inode numbers are entry ids plus one, except that the root is always inode 1.
    fn ino(&self, entry: EntryId) -> InodeNo {
        self.kernel.ino(entry)
    }

    fn entry_id(&self, ino: InodeNo) -> EntryId {
        self.kernel.entry(ino)
    }

    fn mounted(&self, invalidate: impl Fn(Invalidation) + Send + Sync + 'static) {
        self.kernel.mounted(invalidate);
    }

    /// Counts a reply that hands the kernel a reference to the entry's inode, and returns its
    /// attributes for that reply.
    fn looked_up(&self, entry: &Entry, asked: Instant, local: Option<&File>) -> Attributes {
        self.inodes.looked_up(entry, asked);
        self.attr(entry, local)
    }

    /// Records an entry this client just made, whose reply hands the kernel its inode.
    fn added(&self, parent: EntryId, entry: &Entry, asked: Instant) -> Attributes {
        self.inodes.confirm_parent(parent, &entry.path, asked);
        self.inodes.stale(&entry.path);
        let (entry, local) = self.inodes.view(entry.clone());
        self.looked_up(&entry, asked, local.as_deref())
    }

    /// Drops the kernel's `lookups` references to `ino`.
    fn forget(&self, ino: InodeNo, lookups: u64) {
        self.inodes.forget(self.entry_id(ino), lookups);
    }

    /// The entry's attributes, with size and mtime from its journal copy while it has one.
    fn attr(&self, entry: &Entry, local: Option<&File>) -> Attributes {
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
            Kind::File => FileKind::File,
            Kind::Dir => FileKind::Directory,
            Kind::Symlink => FileKind::Symlink,
        };
        Attributes {
            ino: self.ino(entry.id),
            size,
            blocks: size.div_ceil(512),
            time: mtime,
            kind,
            perm: (entry.mode & PERMISSIONS) as u16,
            nlink: if kind == FileKind::Directory { 2 } else { 1 },
            uid: self.owner.0,
            gid: self.owner.1,
            blksize: 4096,
        }
    }

    /// The counters behind `user.herdfs.stats`.
    pub fn stats(&self) -> Vec<u8> {
        self.stats
            .json(self.inodes.count(Local::is_pending), self.remote.round_trip.get())
    }
}

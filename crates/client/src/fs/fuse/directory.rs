use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use fuser::{Errno, FileType, INodeNo, ReplyDirectory};

use crate::fs::{FileKind, InodeNo as SpaceInodeNo, Space};
use crate::{Result, lock};

type Listing = Vec<(SpaceInodeNo, FileKind, String)>;

struct DirectoryEntry {
    ino: SpaceInodeNo,
    kind: FileKind,
    name: String,
    next: u64,
}

#[derive(Clone)]
pub(super) struct Directories {
    listings: Arc<Mutex<HashMap<u64, Listing>>>,
    next: Arc<AtomicU64>,
}

impl Default for Directories {
    fn default() -> Self {
        Self {
            listings: Arc::default(),
            next: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl Directories {
    pub(super) async fn open(&self, space: Arc<Space>, ino: SpaceInodeNo) -> Result<u64> {
        let asked = Instant::now();
        let entries = space.list(ino).await?;
        let listing = entries
            .iter()
            .map(|entry| {
                space.inodes.observe(entry, asked, true);
                let attr = space.attr(entry, None);
                (attr.ino, attr.kind, entry.path.name().to_owned())
            })
            .collect();
        let handle = self.next.fetch_add(1, Relaxed);
        lock(&self.listings).insert(handle, listing);
        Ok(handle)
    }

    pub(super) fn read(&self, handle: u64, offset: u64, mut reply: ReplyDirectory) {
        let listings = lock(&self.listings);
        let Some(listing) = listings.get(&handle) else {
            return reply.error(Errno::EBADF);
        };
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        let entries: Vec<_> = listing
            .iter()
            .enumerate()
            .skip(offset)
            .map(|(next, (ino, kind, name))| DirectoryEntry {
                ino: *ino,
                kind: *kind,
                name: name.clone(),
                next: next as u64 + 1,
            })
            .collect();
        drop(listings);
        for entry in entries {
            let kind = match entry.kind {
                FileKind::File => FileType::RegularFile,
                FileKind::Directory => FileType::Directory,
                FileKind::Symlink => FileType::Symlink,
            };
            if reply.add(INodeNo(entry.ino.0), entry.next, kind, entry.name) {
                break;
            }
        }
        reply.ok();
    }

    pub(super) fn release(&self, handle: u64) {
        lock(&self.listings).remove(&handle);
    }
}

//! Namespace operations: lookups, attributes, directories and renames.

use std::sync::{Arc, atomic::Ordering::Relaxed};
use std::time::{Duration, Instant};
use std::{ffi::OsString, path::PathBuf};

use fuser::{Errno, FileAttr, INodeNo, ReplyDirectory};
use herdfs_core::SpacePath;

use super::{PERMISSIONS, Space, TTL};
use crate::{Result, lock};

pub struct AttrChange {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub mtime_ms: Option<i64>,
    pub fh: Option<u64>,
}

impl Space {
    pub async fn lookup(self: Arc<Self>, parent: INodeNo, name: OsString) -> Result<(FileAttr, Duration)> {
        let (entry, asked, local) = self.entry(&self.located_child(parent, &name).await?).await?;
        let attr = self.looked_up(&entry, asked, local.as_deref());
        Ok((attr, TTL.saturating_sub(asked.elapsed())))
    }

    /// The inode's attributes. If its path names another entry or none, the entry has moved,
    /// or is gone and its name may now be another's, which ESTALE sends the kernel to look up
    /// (C7).
    pub async fn getattr(self: Arc<Self>, ino: INodeNo) -> Result<(FileAttr, Duration)> {
        let found = match self.path(ino)? {
            Some(path) => self.entry(&path).await.map(Some),
            None => Ok(None),
        };
        let (entry, asked, local) = match found {
            Ok(Some(found)) if self.ino(found.0.id) == ino => found,
            Err(err) if !err.is_not_found() => return Err(err),
            _ => {
                let asked = Instant::now();
                let (entry, local) = self.view(self.relocate(ino).await?);
                (entry, asked, local)
            }
        };
        let attr = self.attr(&entry, local.as_deref());
        Ok((attr, TTL.saturating_sub(asked.elapsed())))
    }

    pub async fn setattr(self: Arc<Self>, ino: INodeNo, change: AttrChange) -> Result<FileAttr> {
        let foreign = |id: Option<u32>, own: u32| id.is_some_and(|id| id != own);
        if foreign(change.uid, self.owner.0) || foreign(change.gid, self.owner.1) {
            return Err(Errno::EPERM.into());
        }
        let path = self.located(ino).await?;
        // Answered from the entry held, which offline lets a truncation go on (C12), and found
        // before anything is journaled, so a failure changes nothing.
        let (entry, _, local) = self.entry(&path).await?;
        self.expect(ino, &entry)?;
        if let Some(size) = change.size {
            self.truncate(ino, size, change.fh).await?;
        }
        let mode = change.mode.map(|mode| mode & PERMISSIONS);
        let entry_id = self.entry_id(ino);
        // The next commit keeps a journaled mtime, which it would otherwise stamp over (cp -p).
        let journaled = match change.mtime_ms {
            Some(mtime) => self.journal.set_mtime(entry_id, mtime)?,
            None => false,
        };
        let mtime_ms = change.mtime_ms.filter(|_| !journaled);
        if mtime_ms.is_some() {
            // A commit already under way would stamp its own time over this one.
            self.committed(entry_id).await?;
        }
        if mode.is_none() && mtime_ms.is_none() {
            // A truncation has made the journal copy to show since.
            let (entry, local) = match change.size {
                Some(_) => self.view(entry),
                None => (entry, local),
            };
            return Ok(self.attr(&entry, local.as_deref()));
        }
        let asked = Instant::now();
        let entry = self.remote.set_attr(path, mode, mtime_ms).await?;
        self.inodes.observe(&entry, asked, false);
        self.expect(ino, &entry)?;
        let (entry, local) = self.view(entry);
        Ok(self.attr(&entry, local.as_deref()))
    }

    pub async fn readlink(self: Arc<Self>, ino: INodeNo) -> Result<Vec<u8>> {
        let (entry, ..) = self.entry(&self.located(ino).await?).await?;
        self.expect(ino, &entry)?;
        Ok(entry.target.ok_or(Errno::EINVAL)?.into_bytes())
    }

    pub async fn mkdir(self: Arc<Self>, parent: INodeNo, name: OsString, mode: u32) -> Result<FileAttr> {
        let path = self.located_child(parent, &name).await?;
        let entry = self.remote.mkdir(path, mode & PERMISSIONS).await?;
        Ok(self.added(&entry))
    }

    pub async fn symlink(
        self: Arc<Self>,
        parent: INodeNo,
        name: OsString,
        target: PathBuf,
    ) -> Result<FileAttr> {
        let target = target.into_os_string().into_string().map_err(|_| Errno::EINVAL)?;
        let path = self.located_child(parent, &name).await?;
        let entry = self.remote.symlink(path, target).await?;
        Ok(self.added(&entry))
    }

    /// Unlinks a file or symlink, or removes an empty directory.
    pub async fn remove(self: Arc<Self>, parent: INodeNo, name: OsString) -> Result<()> {
        let path = self.located_child(parent, &name).await?;
        let removed = self.remote.remove(path.clone()).await?;
        // The removed entry's changes are moot: deletion wins (C10), for it alone (C4a).
        self.journal.discard(removed.id);
        self.inodes.stale(&path);
        // Another entry held at that path had moved away: `Locate` tells where (C7).
        self.inodes.unbind(&path);
        self.inodes.removed(removed.id);
        Ok(())
    }

    /// Renames at the authority, atomically there (C7), then moves every known path below.
    pub async fn rename(self: Arc<Self>, from: (INodeNo, OsString), to: (INodeNo, OsString)) -> Result<()> {
        let from = self.located_child(from.0, &from.1).await?;
        let to = self.located_child(to.0, &to.1).await?;
        self.settle(&from).await?;
        self.remote.rename(from.clone(), to.clone()).await?;
        self.inodes.stale(&from);
        self.inodes.stale(&to);
        self.inodes.moved(&from, &to);
        Ok(())
    }

    /// Commits the changes pending at or below `path` before a rename moves them, as fsync does,
    /// so that other clients see the old bytes there or the new, whole (C3, C7): rsync, git and
    /// editors rename a file they closed over the one it replaces.
    async fn settle(&self, path: &SpacePath) -> Result<()> {
        let entries = self.inodes.within(path);
        for &entry in &entries {
            tokio::task::block_in_place(|| self.journal.seal(entry))?;
            self.journal.urge(entry);
        }
        for entry in entries {
            self.committed(entry).await?;
        }
        Ok(())
    }

    /// Lists the directory once, for the readdir calls on this handle to page through.
    pub async fn opendir(self: Arc<Self>, ino: INodeNo) -> Result<u64> {
        let path = self.located(ino).await?;
        let asked = Instant::now();
        let entries = self.remote.list(path).await?;
        let listed = |entry| {
            self.inodes.observe(entry, asked, true);
            let attr = self.attr(entry, None);
            (attr.ino, attr.kind, entry.path.name().to_owned())
        };
        let listing = entries.iter().map(listed).collect();
        let handle = self.next_handle.fetch_add(1, Relaxed);
        lock(&self.listings).insert(handle, listing);
        Ok(handle)
    }

    pub fn readdir(&self, handle: u64, offset: u64, mut reply: ReplyDirectory) {
        let listings = lock(&self.listings);
        let Some(listing) = listings.get(&handle) else {
            return reply.error(Errno::EBADF);
        };
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        for (next, (ino, kind, name)) in listing.iter().enumerate().skip(offset) {
            if reply.add(*ino, next as u64 + 1, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    pub fn releasedir(&self, handle: u64) {
        lock(&self.listings).remove(&handle);
    }
}

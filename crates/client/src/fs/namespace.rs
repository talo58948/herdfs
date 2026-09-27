//! Namespace operations: lookups, attributes, directories and renames.

use std::sync::{Arc, atomic::Ordering::Relaxed};
use std::{ffi::OsString, path::PathBuf, time::Duration};

use fuser::{Errno, FileAttr, INodeNo, ReplyDirectory};

use super::{PERMISSIONS, Space};
use crate::{error::Error, lock};

/// The parts of a setattr that herdfs stores.
pub struct AttrChange {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub mtime_ms: Option<i64>,
    pub fh: Option<u64>,
}

impl Space {
    pub async fn lookup(
        self: Arc<Self>,
        parent: INodeNo,
        name: OsString,
    ) -> Result<(FileAttr, Duration), Error> {
        let (entry, ttl) = self.entry(&self.child(parent, &name)?).await?;
        Ok((self.looked_up(&entry), ttl))
    }

    pub async fn getattr(self: Arc<Self>, ino: INodeNo) -> Result<(FileAttr, Duration), Error> {
        let (entry, ttl) = self.entry(&self.path(ino)?).await?;
        // The path may name a different entry by now.
        if self.ino(entry.id) != ino {
            return Err(Errno::ENOENT.into());
        }
        Ok((self.attr(&entry), ttl))
    }

    pub async fn setattr(self: Arc<Self>, ino: INodeNo, change: AttrChange) -> Result<FileAttr, Error> {
        let foreign = |id: Option<u32>, own: u32| id.is_some_and(|id| id != own);
        if foreign(change.uid, self.owner.0) || foreign(change.gid, self.owner.1) {
            return Err(Errno::EPERM.into());
        }
        if let Some(size) = change.size {
            self.truncate(ino, size, change.fh).await?;
        }
        let (path, mode) = (self.path(ino)?, change.mode.map(|mode| mode & PERMISSIONS));
        let entry = if mode.is_some() || change.mtime_ms.is_some() {
            // A commit landing later would set the mtime to its own time.
            self.committed(self.entry_id(ino)).await?;
            self.remote.set_attr(path, mode, change.mtime_ms).await?
        } else {
            self.remote.stat(path).await?
        };
        self.remember(&entry);
        Ok(self.attr(&entry))
    }

    pub async fn readlink(self: Arc<Self>, ino: INodeNo) -> Result<Vec<u8>, Error> {
        let (entry, _) = self.entry(&self.path(ino)?).await?;
        Ok(entry.target.ok_or(Errno::EINVAL)?.into_bytes())
    }

    pub async fn mkdir(
        self: Arc<Self>,
        parent: INodeNo,
        name: OsString,
        mode: u32,
    ) -> Result<FileAttr, Error> {
        let path = self.child(parent, &name)?;
        let entry = self.remote.mkdir(path, mode & PERMISSIONS).await?;
        Ok(self.added(&entry))
    }

    pub async fn symlink(
        self: Arc<Self>,
        parent: INodeNo,
        name: OsString,
        target: PathBuf,
    ) -> Result<FileAttr, Error> {
        let target = target.into_os_string().into_string().map_err(|_| Errno::EINVAL)?;
        let entry = self.remote.symlink(self.child(parent, &name)?, target).await?;
        Ok(self.added(&entry))
    }

    /// Unlinks a file or symlink, or removes an empty directory.
    pub async fn remove(self: Arc<Self>, parent: INodeNo, name: OsString) -> Result<(), Error> {
        let path = self.child(parent, &name)?;
        self.remote.remove(path.clone()).await?;
        self.stale(&path);
        lock(&self.paths).retain(|_, (known, _)| *known != path);
        Ok(())
    }

    /// Renames at the authority, atomically there (C7), then moves every known path below.
    pub async fn rename(
        self: Arc<Self>,
        from: (INodeNo, OsString),
        to: (INodeNo, OsString),
    ) -> Result<(), Error> {
        let (from, to) = (self.child(from.0, &from.1)?, self.child(to.0, &to.1)?);
        self.remote.rename(from.clone(), to.clone()).await?;
        self.stale(&from);
        self.stale(&to);
        let mut paths = lock(&self.paths);
        paths.retain(|_, (known, _)| *known != to);
        for (known, _) in paths.values_mut() {
            if let Some(moved) = known.rebase(&from, &to) {
                *known = moved;
            }
        }
        Ok(())
    }

    /// Lists the directory once, for the readdir calls on this handle to page through.
    pub async fn opendir(self: Arc<Self>, ino: INodeNo) -> Result<u64, Error> {
        let entries = self.remote.list(self.path(ino)?).await?;
        let listed = |entry| {
            self.remember(entry);
            let attr = self.attr(entry);
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

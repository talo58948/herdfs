//! Namespace operations: lookups, attributes, directories and renames.

use std::sync::{Arc, atomic::Ordering::Relaxed};
use std::time::{Duration, Instant};
use std::{ffi::OsString, path::PathBuf};

use fuser::{Errno, FileAttr, INodeNo, ReplyDirectory};

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
        let (entry, fetched) = self.entry(&self.located_child(parent, &name).await?).await?;
        Ok((
            self.looked_up(&entry, fetched),
            TTL.saturating_sub(fetched.elapsed()),
        ))
    }

    /// The inode's attributes. If its path names another entry or none, the entry has moved,
    /// or is gone and its name may now be another's, which ESTALE sends the kernel to look up
    /// (C7).
    pub async fn getattr(self: Arc<Self>, ino: INodeNo) -> Result<(FileAttr, Duration)> {
        let (entry, fetched) = match self.entry(&self.path(ino)?).await {
            Ok((entry, fetched)) if self.ino(entry.id) == ino => (entry, fetched),
            Err(err) if !err.is_not_found() => return Err(err),
            _ => (self.relocate(ino).await?, Instant::now()),
        };
        Ok((self.attr(&entry), TTL.saturating_sub(fetched.elapsed())))
    }

    pub async fn setattr(self: Arc<Self>, ino: INodeNo, change: AttrChange) -> Result<FileAttr> {
        let foreign = |id: Option<u32>, own: u32| id.is_some_and(|id| id != own);
        if foreign(change.uid, self.owner.0) || foreign(change.gid, self.owner.1) {
            return Err(Errno::EPERM.into());
        }
        let path = self.located(ino).await?;
        // Answered from the entry held, which offline lets a truncation go on (C12), and found
        // before anything is journaled, so a failure changes nothing.
        let (entry, _) = self.entry(&path).await?;
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
            return Ok(self.attr(&entry));
        }
        let entry = self.remote.set_attr(path, mode, mtime_ms).await?;
        self.remember(&entry);
        Ok(self.attr(&entry))
    }

    pub async fn readlink(self: Arc<Self>, ino: INodeNo) -> Result<Vec<u8>> {
        let (entry, _) = self.entry(&self.located(ino).await?).await?;
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
        self.remote.remove(path.clone()).await?;
        self.stale(&path);
        lock(&self.paths).retain(|_, (known, ..)| *known != path);
        Ok(())
    }

    /// Renames at the authority, atomically there (C7), then moves every known path below.
    pub async fn rename(self: Arc<Self>, from: (INodeNo, OsString), to: (INodeNo, OsString)) -> Result<()> {
        let from = self.located_child(from.0, &from.1).await?;
        let to = self.located_child(to.0, &to.1).await?;
        self.remote.rename(from.clone(), to.clone()).await?;
        self.stale(&from);
        self.stale(&to);
        for (known, _, confirmed) in lock(&self.paths).values_mut() {
            // Replaced, unless another client had moved it away: `Locate` tells (C7).
            if *known == to {
                *confirmed = None;
            } else if let Some(moved) = known.rebase(&from, &to) {
                *known = moved;
            }
        }
        Ok(())
    }

    /// Lists the directory once, for the readdir calls on this handle to page through.
    pub async fn opendir(self: Arc<Self>, ino: INodeNo) -> Result<u64> {
        let path = self.located(ino).await?;
        let asked = Instant::now();
        let entries = self.remote.list(path).await?;
        let listed = |entry| {
            self.listed(entry, asked);
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

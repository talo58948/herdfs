//! Namespace operations: lookups, attributes, directories and renames.

use std::sync::{Arc, atomic::Ordering::Relaxed};
use std::time::{Duration, Instant};
use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use herdfs_core::SpacePath;
use herdfs_core::proto::{EntryId, ErrorCode};

use super::{Attributes, DirectoryEntry, InodeNo, MAX_NAME, OpenMode, PERMISSIONS, Space, handles::Handle};
use crate::{Error, Result, journal::Local, lock};

pub struct AttrChange {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub mtime_ms: Option<i64>,
    pub fh: Option<u64>,
}

impl Space {
    /// Creates an empty file and opens it for writing. If another client created the name
    /// first and O_EXCL was not asked for, opens that file instead, as O_CREAT does (C5).
    pub async fn create(
        self: Arc<Self>,
        (parent, name): (InodeNo, OsString),
        mode: u32,
        exclusive: bool,
        open_mode: OpenMode,
        comm: &str,
    ) -> Result<(Attributes, u64)> {
        let parent_id = self.entry_id(parent);
        let name = child_name(&name)?;
        let asked = Instant::now();
        let created = self
            .remote
            .create(parent_id, name.clone(), mode & PERMISSIONS)
            .await;
        let entry = match created {
            Err(Error::Api(error)) if error.code == ErrorCode::Exists && !exclusive => {
                let (entry, asked, local) = self.child(parent, name).await?;
                let attr = self.looked_up(&entry, asked, local.as_deref());
                let open = self.clone().open(self.ino(entry.id), open_mode, comm);
                return Ok((attr, open.await?));
            }
            created => created?,
        };
        let attr = self.added(parent_id, &entry, asked);
        self.inodes.with(entry.id, |state| state.switch((None, true)));
        let fh = self.register(Handle::new(entry.id, true, None, &self.inodes));
        Ok((attr, fh))
    }

    pub async fn lookup(self: Arc<Self>, parent: InodeNo, name: OsString) -> Result<(Attributes, Duration)> {
        let (entry, asked, local) = self.child(parent, child_name(&name)?).await?;
        let attr = self.looked_up(&entry, asked, local.as_deref());
        Ok((attr, self.metadata_ttl.saturating_sub(asked.elapsed())))
    }

    /// The held inode's attributes, wherever another client has moved it (C7).
    pub async fn getattr(self: Arc<Self>, ino: InodeNo) -> Result<(Attributes, Duration)> {
        let (entry, asked, local) = self.entry(ino).await?;
        let attr = self.attr(&entry, local.as_deref());
        Ok((attr, self.metadata_ttl.saturating_sub(asked.elapsed())))
    }

    pub async fn setattr(self: Arc<Self>, ino: InodeNo, change: AttrChange) -> Result<Attributes> {
        let foreign = |id: Option<u32>, own: u32| id.is_some_and(|id| id != own);
        if foreign(change.uid, self.owner.0) || foreign(change.gid, self.owner.1) {
            return Err(libc::EPERM.into());
        }
        // Answered from the entry held, which offline lets a truncation go on (C12), and found
        // before anything is journaled, so a failure changes nothing.
        let (entry, ..) = self.entry(ino).await?;
        if let Some(size) = change.size {
            self.truncate(ino, size, change.fh).await?;
        }
        let mode = change.mode.map(|mode| mode & PERMISSIONS);
        let entry_id = self.entry_id(ino);
        // The next commit keeps a journaled mtime, which it would otherwise stamp over (cp -p).
        let journaled = match change.mtime_ms {
            Some(mtime) => {
                let set = |local: &mut Local| local.set_mtime(&self.journal, entry_id, mtime);
                self.inodes.local(entry_id, set).unwrap_or(Ok(false))?
            }
            None => false,
        };
        let mtime_ms = change.mtime_ms.filter(|_| !journaled);
        if mtime_ms.is_some() {
            // A commit already under way would stamp its own time over this one.
            self.committed(entry_id).await?;
        }
        let entry = if mode.is_some() || mtime_ms.is_some() {
            let asked = Instant::now();
            let entry = self.remote.set_attr(entry_id, mode, mtime_ms).await?;
            self.inodes.observe(&entry, asked, false);
            entry
        } else {
            entry
        };
        // The inode as it stands now, with any journal copy the truncation made (C1).
        let (entry, local) = self.inodes.view(entry);
        Ok(self.attr(&entry, local.as_deref()))
    }

    pub async fn readlink(self: Arc<Self>, ino: InodeNo) -> Result<Vec<u8>> {
        let (entry, ..) = self.entry(ino).await?;
        Ok(entry.target.ok_or(libc::EINVAL)?.into_bytes())
    }

    pub async fn mkdir(self: Arc<Self>, parent: InodeNo, name: OsString, mode: u32) -> Result<Attributes> {
        let parent_id = self.entry_id(parent);
        let asked = Instant::now();
        let entry = self
            .remote
            .mkdir(parent_id, child_name(&name)?, mode & PERMISSIONS)
            .await?;
        Ok(self.added(parent_id, &entry, asked))
    }

    pub async fn symlink(
        self: Arc<Self>,
        parent: InodeNo,
        name: OsString,
        target: PathBuf,
    ) -> Result<Attributes> {
        let target = target.into_os_string().into_string().map_err(|_| libc::EINVAL)?;
        let parent_id = self.entry_id(parent);
        let asked = Instant::now();
        let entry = self.remote.symlink(parent_id, child_name(&name)?, target).await?;
        Ok(self.added(parent_id, &entry, asked))
    }

    /// Unlinks a file or symlink, or removes an empty directory.
    pub async fn remove(self: Arc<Self>, parent: InodeNo, name: OsString) -> Result<()> {
        let parent_id = self.entry_id(parent);
        let removed = self.remote.remove(parent_id, child_name(&name)?).await?;
        let path = &removed.path;
        // The removed entry's changes are moot: deletion wins (C10), for it alone (C4a).
        let discard = |local: &mut Local| local.discard(&self.journal, removed.id);
        self.inodes.local(removed.id, discard);
        self.inodes.stale(path);
        // Another entry held at that path had moved away: `Locate` tells where (C7).
        self.inodes.unbind(path);
        self.inodes.removed(removed.id);
        Ok(())
    }

    /// Renames at the authority, atomically there (C7), then moves every known path below.
    pub async fn rename(
        self: Arc<Self>,
        from: (InodeNo, OsString),
        to: (InodeNo, OsString),
    ) -> Result<Option<EntryId>> {
        let parents = (self.entry_id(from.0), self.entry_id(to.0));
        let (from_name, to_name) = (child_name(&from.1)?, child_name(&to.1)?);
        let held = self.fresh_child(from.0, &from_name).await?;
        let asked = Instant::now();
        let current = self.remote.lookup(parents.0, from_name.clone()).await?;
        self.follow_child(parents.0, &current, asked).await;
        let source = match held {
            Some(held) if held.id != current.id => return Err(libc::ESTALE.into()),
            _ => current,
        };
        self.settle(&source.path).await?;
        let asked = Instant::now();
        let (from, moved) = self
            .remote
            .rename(parents.0, from_name, parents.1, to_name)
            .await?;
        self.inodes.stale(&from);
        self.inodes.stale(&moved.path);
        self.inodes.moved(parents.1, &from, &moved, asked);
        Ok((source.id != moved.id).then_some(source.id))
    }

    /// Commits the changes pending at or below `path` before a rename moves them, as fsync does,
    /// so that other clients see the old bytes there or the new, whole (C3, C7): rsync, git and
    /// editors rename a file they closed over the one it replaces.
    async fn settle(&self, path: &SpacePath) -> Result<()> {
        let entries = self.inodes.within(path);
        for &entry in &entries {
            tokio::task::block_in_place(|| self.seal_entry(entry))?;
            self.urge(entry);
        }
        for entry in entries {
            self.committed(entry).await?;
        }
        Ok(())
    }

    /// Lists the directory once, for the readdir calls on this handle to page through.
    pub async fn opendir(self: Arc<Self>, ino: InodeNo) -> Result<u64> {
        let asked = Instant::now();
        let entries = self.remote.list(self.entry_id(ino)).await?;
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

    pub fn readdir(&self, handle: u64, offset: u64) -> Result<Vec<DirectoryEntry>> {
        let listings = lock(&self.listings);
        let listing = listings.get(&handle).ok_or(libc::EBADF)?;
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        Ok(listing
            .iter()
            .enumerate()
            .skip(offset)
            .map(|(next, (ino, kind, name))| DirectoryEntry {
                ino: *ino,
                kind: *kind,
                name: name.clone(),
                next: next as u64 + 1,
            })
            .collect())
    }

    pub fn releasedir(&self, handle: u64) {
        lock(&self.listings).remove(&handle);
    }
}

fn child_name(name: &OsStr) -> Result<String> {
    if name.len() > MAX_NAME {
        return Err(libc::ENAMETOOLONG.into());
    }
    let name = name.to_str().ok_or(libc::EINVAL)?;
    let path = SpacePath::try_from(name).map_err(|_| libc::EINVAL)?;
    if path.parent() != Some(SpacePath::root()) {
        return Err(libc::EINVAL.into());
    }
    Ok(name.to_owned())
}

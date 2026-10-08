//! Namespace operations shared by the NFS trait adapter.

use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use super::{Nfs, unix_ms};
use crate::fs::{OpenMode, namespace::AttrChange};
use nfsserve::nfs::{self, nfsstat3};
use nfsserve::vfs::{DirEntry, ReadDirResult};

impl Nfs {
    pub(super) async fn lookup_entry(&self, dir: u64, name: &nfs::filename3) -> Result<u64, nfsstat3> {
        let (attr, _) = self
            .space
            .clone()
            .lookup(self.ino(dir), Self::name(name)?)
            .await
            .map_err(|error| Self::status(&error))?;
        let id = Self::id(self.space.entry_id(attr.ino));
        self.space.forget(attr.ino, 1);
        Ok(id)
    }

    pub(super) async fn get_attr(&self, id: u64) -> Result<nfs::fattr3, nfsstat3> {
        let entry = Self::entry(id);
        let attr = match self.epoch_attr(entry).await {
            Some(attr) => attr,
            None => {
                self.space
                    .clone()
                    .getattr(self.ino(id))
                    .await
                    .map_err(|error| Self::status(&error))?
                    .0
            }
        };
        Ok(self.attr(&attr))
    }

    pub(super) async fn set_attr(&self, id: u64, attr: nfs::sattr3) -> Result<nfs::fattr3, nfsstat3> {
        self.change_attr(id, attr, None).await
    }

    async fn change_attr(
        &self,
        id: u64,
        attr: nfs::sattr3,
        handle: Option<u64>,
    ) -> Result<nfs::fattr3, nfsstat3> {
        let mode = match attr.mode {
            nfs::set_mode3::mode(mode) => Some(mode),
            nfs::set_mode3::Void => None,
        };
        let uid = match attr.uid {
            nfs::set_uid3::uid(uid) => Some(uid),
            nfs::set_uid3::Void => None,
        };
        let gid = match attr.gid {
            nfs::set_gid3::gid(gid) => Some(gid),
            nfs::set_gid3::Void => None,
        };
        let size = match attr.size {
            nfs::set_size3::size(size) => Some(size),
            nfs::set_size3::Void => None,
        };
        let mtime_ms = match attr.mtime {
            nfs::set_mtime::DONT_CHANGE => None,
            nfs::set_mtime::SET_TO_SERVER_TIME => Some(crate::unix_ms(SystemTime::now())),
            nfs::set_mtime::SET_TO_CLIENT_TIME(time) => Some(unix_ms(time)),
        };
        let change = AttrChange {
            mode,
            uid,
            gid,
            size,
            mtime_ms,
            fh: handle,
        };
        let attr = self
            .space
            .clone()
            .setattr(self.ino(id), change)
            .await
            .map_err(|error| Self::status(&error))?;
        if size.is_some() {
            let flow = self.flow(Self::entry(id));
            let mut state = flow.lock().await;
            let epoch = state.epoch.take();
            state.last_data = Some(Instant::now());
            drop(state);
            if let Some(epoch) = epoch {
                self.space
                    .clone()
                    .release(epoch.handle)
                    .await
                    .map_err(|error| Self::status(&error))?;
            }
        }
        Ok(self.attr(&attr))
    }

    pub(super) async fn create_file(
        &self,
        dir: u64,
        name: &nfs::filename3,
        attr: nfs::sattr3,
        guarded: bool,
    ) -> Result<(u64, nfs::fattr3), nfsstat3> {
        let mode = match attr.mode {
            nfs::set_mode3::mode(mode) => mode,
            nfs::set_mode3::Void => 0o666,
        };
        let (made, handle) = self
            .space
            .clone()
            .create(
                (self.ino(dir), Self::name(name)?),
                mode,
                guarded,
                OpenMode::Write,
                0,
            )
            .await
            .map_err(|error| Self::status(&error))?;
        let id = Self::id(self.space.entry_id(made.ino));
        let changed = self.change_attr(id, attr, Some(handle)).await;
        let released = self.space.clone().release(handle).await;
        let attr = changed?;
        released.map_err(|error| Self::status(&error))?;
        Ok((id, attr))
    }

    pub(super) async fn create_exclusive_file(
        &self,
        dir: u64,
        name: &nfs::filename3,
        verifier: [u8; 8],
    ) -> Result<u64, nfsstat3> {
        let parent = Self::entry(dir);
        let name = Self::name(name)?;
        let text = name.to_str().ok_or(nfsstat3::NFS3ERR_INVAL)?.to_owned();
        if let Some(entry) = self.identity.replay(parent, &text, verifier)
            && self.get_attr(Self::id(entry)).await.is_ok()
        {
            return Ok(Self::id(entry));
        }
        let (attr, handle) = self
            .space
            .clone()
            .create((self.ino(dir), name), 0o666, true, OpenMode::Write, 0)
            .await
            .map_err(|error| Self::status(&error))?;
        self.space
            .clone()
            .release(handle)
            .await
            .map_err(|error| Self::status(&error))?;
        let entry = self.space.entry_id(attr.ino);
        self.identity
            .remember(parent, text, verifier, entry)
            .map_err(|error| Self::status(&error.into()))?;
        Ok(Self::id(entry))
    }

    pub(super) async fn make_dir(
        &self,
        dir: u64,
        name: &nfs::filename3,
        attr: nfs::sattr3,
    ) -> Result<(u64, nfs::fattr3), nfsstat3> {
        let mode = match attr.mode {
            nfs::set_mode3::mode(mode) => mode,
            nfs::set_mode3::Void => 0o777,
        };
        let attr = self
            .space
            .clone()
            .mkdir(self.ino(dir), Self::name(name)?, mode)
            .await
            .map_err(|error| Self::status(&error))?;
        Ok((Self::id(self.space.entry_id(attr.ino)), self.attr(&attr)))
    }

    pub(super) async fn make_symlink(
        &self,
        dir: u64,
        name: &nfs::filename3,
        target: &nfs::nfspath3,
        changes: nfs::sattr3,
    ) -> Result<(u64, nfs::fattr3), nfsstat3> {
        let target = std::str::from_utf8(target.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let attr = self
            .space
            .clone()
            .symlink(self.ino(dir), Self::name(name)?, PathBuf::from(target))
            .await
            .map_err(|error| Self::status(&error))?;
        let id = Self::id(self.space.entry_id(attr.ino));
        Ok((id, self.change_attr(id, changes, None).await?))
    }

    pub(super) async fn remove_name(
        &self,
        dir: u64,
        name: &nfs::filename3,
        directory: bool,
    ) -> Result<(), nfsstat3> {
        let id = self.lookup_entry(dir, name).await?;
        let attr = self.get_attr(id).await?;
        match (directory, attr.ftype) {
            (true, nfs::ftype3::NF3DIR) | (false, nfs::ftype3::NF3REG | nfs::ftype3::NF3LNK) => {}
            (true, _) => return Err(nfsstat3::NFS3ERR_NOTDIR),
            (false, _) => return Err(nfsstat3::NFS3ERR_ISDIR),
        }
        self.space
            .clone()
            .remove(self.ino(dir), Self::name(name)?)
            .await
            .map_err(|error| Self::status(&error))
    }

    pub(super) async fn list_dir(
        &self,
        dir: u64,
        cookie: u64,
        count: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let asked = Instant::now();
        let entries = self
            .space
            .remote
            .list(Self::entry(dir))
            .await
            .map_err(|error| Self::status(&error))?;
        let start = entries
            .iter()
            .position(|entry| entry.id.0 == cookie)
            .map_or(0, |position| position + 1);
        let total = entries.len();
        let end = start.saturating_add(count).min(total);
        let mut listed = Vec::with_capacity(end - start);
        for entry in &entries[start..end] {
            self.space.inodes.observe(entry, asked, true);
            let attr = match self.epoch_attr(entry.id).await {
                Some(attr) => attr,
                None => {
                    let (entry, local) = self.space.inodes.view(entry.clone());
                    self.space.attr(&entry, local.as_deref())
                }
            };
            listed.push(DirEntry {
                fileid: entry.id.0,
                name: entry.path.name().as_bytes().into(),
                attr: self.attr(&attr),
            });
        }
        Ok(ReadDirResult {
            entries: listed,
            end: end == total,
        })
    }
}

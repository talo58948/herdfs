//! Namespace operations shared by the NFS trait adapter.

use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use super::{Nfs, nfs_unix_ms};
use crate::Error;
use crate::fs::{FileKind, OpenMode, namespace::AttrChange};
use nfsserve::nfs::{self, nfsstat3};
use nfsserve::vfs::{DirEntry, ReadDirResult};

impl Nfs {
    pub(super) async fn lookup_entry(&self, dir: u64, name: &nfs::filename3) -> Result<u64, nfsstat3> {
        let found = self.space.clone().lookup(self.ino(dir), Self::name(name)?).await;
        let (attr, _) = match found {
            Ok(found) => found,
            Err(error) if error.errno() == libc::ENOENT => {
                return Err(self.name_error(&[dir], error).await);
            }
            Err(error) => return Err(error.into()),
        };
        let id = self.space.entry_id(attr.ino).0;
        self.space.forget(attr.ino, 1);
        Ok(id)
    }

    pub(super) async fn get_attr(&self, id: u64) -> Result<nfs::fattr3, nfsstat3> {
        let entry = herdfs_core::proto::EntryId(id);
        let attr = match self.flow_attr(entry).await {
            Some(attr) => attr,
            None => {
                self.space
                    .clone()
                    .getattr(self.ino(id))
                    .await
                    .map_err(Self::stale)?
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
            nfs::set_mtime::SET_TO_CLIENT_TIME(time) => Some(nfs_unix_ms(time)),
        };
        let change = AttrChange {
            mode,
            uid,
            gid,
            size,
            mtime_ms,
            fh: handle,
        };
        let entry = herdfs_core::proto::EntryId(id);
        let attr = match size {
            Some(size) => self
                .size_generation(entry, size, change, handle)
                .await
                .map_err(Self::stale)?,
            None => self
                .space
                .clone()
                .setattr(self.ino(id), change)
                .await
                .map_err(Self::stale)?,
        };
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
                "nfs",
            )
            .await
            .map_err(Self::stale)?;
        let id = self.space.entry_id(made.ino).0;
        let changes = without_mode(attr);
        if !changes_attr(changes) {
            self.space.clone().release(handle).await?;
            return Ok((id, self.attr(&made)));
        }
        let adopts = matches!(changes.size, nfs::set_size3::size(_));
        let changed = self.change_attr(id, changes, Some(handle)).await;
        if !adopts {
            self.space.clone().release(handle).await?;
        }
        Ok((id, changed?))
    }

    pub(super) async fn create_exclusive_file(
        &self,
        dir: u64,
        name: &nfs::filename3,
        verifier: [u8; 8],
    ) -> Result<u64, nfsstat3> {
        let parent = herdfs_core::proto::EntryId(dir);
        let name = Self::name(name)?;
        let text = name.to_str().ok_or(nfsstat3::NFS3ERR_INVAL)?.to_owned();
        if let Some(entry) = self.identity.replay(parent, &text, verifier)
            && self.get_attr(entry.0).await.is_ok()
        {
            return Ok(entry.0);
        }
        let (attr, handle) = self
            .space
            .clone()
            .create((self.ino(dir), name), 0o666, true, OpenMode::Write, "nfs")
            .await
            .map_err(Self::stale)?;
        self.space.clone().release(handle).await?;
        let entry = self.space.entry_id(attr.ino);
        self.identity
            .remember(parent, text, verifier, entry)
            .map_err(crate::Error::from)?;
        Ok(entry.0)
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
            .map_err(Self::stale)?;
        Ok((self.space.entry_id(attr.ino).0, self.attr(&attr)))
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
            .map_err(Self::stale)?;
        let id = self.space.entry_id(attr.ino).0;
        Ok((id, self.change_attr(id, changes, None).await?))
    }

    pub(super) async fn remove_name(
        &self,
        dir: u64,
        name: &nfs::filename3,
        directory: bool,
    ) -> Result<(), nfsstat3> {
        let name = Self::name(name)?;
        let found = self.space.clone().lookup(self.ino(dir), name.clone()).await;
        let (attr, _) = match found {
            Ok(found) => found,
            Err(error) => return Err(self.name_error(&[dir], error).await),
        };
        let entry = self.space.entry_id(attr.ino);
        self.space.forget(attr.ino, 1);
        match (directory, attr.kind) {
            (true, FileKind::Directory) | (false, FileKind::File | FileKind::Symlink) => {}
            (true, _) => return Err(nfsstat3::NFS3ERR_NOTDIR),
            (false, _) => return Err(nfsstat3::NFS3ERR_ISDIR),
        }
        if let Err(error) = self.space.clone().remove(self.ino(dir), name).await {
            return Err(self.name_error(&[dir], error).await);
        }
        self.discard_flow(entry).await;
        Ok(())
    }

    pub(super) async fn list_dir(
        &self,
        dir: u64,
        cookie: u64,
        count: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let asked = Instant::now();
        let entries = self.space.list(self.ino(dir)).await.map_err(Self::stale)?;
        let start = entries
            .iter()
            .position(|entry| entry.id.0 == cookie)
            .map_or(0, |position| position + 1);
        let total = entries.len();
        let end = start.saturating_add(count).min(total);
        let mut listed = Vec::with_capacity(end - start);
        for entry in &entries[start..end] {
            self.space.inodes.observe(entry, asked, true);
            let attr = match self.flow_attr(entry.id).await {
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

    pub(super) async fn name_error(&self, dirs: &[u64], error: Error) -> nfsstat3 {
        if error.errno() != libc::ENOENT {
            return error.into();
        }
        for dir in dirs {
            if let Err(error) = self
                .space
                .remote
                .locate(herdfs_core::proto::EntryId(*dir))
                .await
            {
                return Self::stale(error);
            }
        }
        nfsstat3::NFS3ERR_NOENT
    }
}

fn without_mode(mut attr: nfs::sattr3) -> nfs::sattr3 {
    attr.mode = nfs::set_mode3::Void;
    attr
}

fn changes_attr(attr: nfs::sattr3) -> bool {
    !matches!(attr.uid, nfs::set_uid3::Void)
        || !matches!(attr.gid, nfs::set_gid3::Void)
        || !matches!(attr.size, nfs::set_size3::Void)
        || !matches!(attr.atime, nfs::set_atime::DONT_CHANGE)
        || !matches!(attr.mtime, nfs::set_mtime::DONT_CHANGE)
}

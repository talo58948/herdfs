//! The `fuser` adapter. Callbacks that may wait on the network hand their work to the tokio
//! runtime and reply from the task, so one slow download never stalls other operations.
//! fuser's defaults answer everything not implemented here: ENOSYS, and EPERM for hard links.

use std::time::SystemTime;
use std::{ffi::OsStr, io, path::Path, sync::Arc};

use fuser::{
    BsdFileFlags, Errno, FileHandle, Filesystem, INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use tokio::runtime::Handle as Runtime;

use super::answer::Answer;
use super::{Space, namespace::AttrChange};
use crate::{error::Error, unix_ms};

const STATS_XATTR: &str = "user.herdfs.stats";

pub struct Fuse {
    space: Arc<Space>,
    runtime: Runtime,
}

impl Fuse {
    pub fn new(space: Arc<Space>, runtime: Runtime) -> Self {
        Self { space, runtime }
    }

    fn space(&self) -> Arc<Space> {
        self.space.clone()
    }

    /// Runs `work` on the runtime and answers with its value or its error's errno.
    fn spawn<T>(
        &self,
        reply: impl Answer<T> + Send + 'static,
        work: impl Future<Output = Result<T, Error>> + Send + 'static,
    ) {
        self.runtime.spawn(async move {
            match work.await {
                Ok(value) => reply.answer(value),
                Err(err) => {
                    tracing::debug!(%err, "request failed");
                    reply.fail(err.errno());
                }
            }
        });
    }
}

impl Filesystem for Fuse {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // Otherwise a truncating open would first download the head only to cut it off.
        if let Err(missing) = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC) {
            tracing::warn!(?missing, "kernel lacks atomic O_TRUNC");
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.spawn(reply, self.space().lookup(parent, name.to_owned()));
    }

    fn forget(&self, _req: &Request, ino: INodeNo, lookups: u64) {
        self.space.forget(ino, lookups);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.spawn(reply, self.space().getattr(ino));
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mtime_ms = mtime.map(|mtime| match mtime {
            TimeOrNow::SpecificTime(time) => unix_ms(time),
            TimeOrNow::Now => unix_ms(SystemTime::now()),
        });
        let fh = fh.map(u64::from);
        let change = AttrChange {
            mode,
            uid,
            gid,
            size,
            mtime_ms,
            fh,
        };
        self.spawn(reply, self.space().setattr(ino, change));
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        self.spawn(reply, self.space().readlink(ino));
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        self.spawn(reply, self.space().mkdir(parent, name.to_owned(), mode & !umask));
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // Devices, FIFOs and sockets are out of v0; regular files come through create.
        reply.error(Errno::EPERM);
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.rmdir(req, parent, name, reply);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.spawn(reply, self.space().remove(parent, name.to_owned()));
    }

    fn symlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, target: &Path, reply: ReplyEntry) {
        self.spawn(
            reply,
            self.space().symlink(parent, name.to_owned(), target.to_owned()),
        );
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        // RENAME_NOREPLACE and RENAME_EXCHANGE cannot be made atomic over the protocol.
        if !flags.is_empty() {
            return reply.error(Errno::EINVAL);
        }
        let (from, to) = ((parent, name.to_owned()), (new_parent, new_name.to_owned()));
        self.spawn(reply, self.space().rename(from, to));
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.spawn(reply, self.space().open(ino, flags, req.pid()));
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.spawn(reply, self.space().read(fh.0, offset, size));
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        // Journal writes are local, so they are answered right here.
        match self.space.write(fh.0, offset, data) {
            Ok(written) => reply.written(written),
            Err(err) => reply.error(err.errno()),
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _owner: LockOwner, reply: ReplyEmpty) {
        self.spawn(reply, self.space().flush(fh.0));
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.spawn(reply, self.space().release(fh.0));
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        self.spawn(reply, self.space().fsync(fh.0));
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.spawn(reply, self.space().opendir(ino));
    }

    fn readdir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectory) {
        self.space.readdir(fh.0, offset, reply);
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.space.releasedir(fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // The client does not bound space; report plenty so tools do not refuse to write.
        reply.statfs(1 << 30, 1 << 30, 1 << 30, 1 << 20, 1 << 20, 4096, 255, 4096);
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        if ino != INodeNo::ROOT || name != STATS_XATTR {
            return reply.error(Errno::NO_XATTR);
        }
        xattr(&self.space.stats(), size, reply);
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let names = [STATS_XATTR.as_bytes(), b"\0"].concat();
        let names = if ino == INodeNo::ROOT { &names[..] } else { &[] };
        xattr(names, size, reply);
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        self.spawn(reply, self.space().create(parent, name.to_owned(), mode & !umask));
    }
}

/// Answers an xattr read: its size when asked with size 0, ERANGE when it does not fit.
fn xattr(value: &[u8], size: u32, reply: ReplyXattr) {
    match u32::try_from(value.len()) {
        Ok(len) if size == 0 => reply.size(len),
        Ok(len) if len <= size => reply.data(value),
        _ => reply.error(Errno::ERANGE),
    }
}

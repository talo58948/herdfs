//! How each FUSE reply carries an operation's value, or its error's errno.

use std::time::Duration;

use fuser::{
    Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation, INodeNo, Notifier, OpenAccMode, OpenFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite,
    ReplyXattr,
};

use super::{Attributes, DirectoryEntry, FileKind, Invalidation, OpenMode, Space, TTL};
use crate::Result;

const GENERATION: Generation = Generation(0);

/// A reply that carries a `T` on success and an errno otherwise.
pub trait Answer<T> {
    fn answer(self, value: T);
    fn fail(self, errno: i32);
}

pub(super) fn reply<T>(reply: impl Answer<T>, result: Result<T>) -> bool {
    match result {
        Ok(value) => {
            reply.answer(value);
            true
        }
        Err(err) => {
            tracing::debug!(%err, "request failed");
            reply.fail(err.errno());
            false
        }
    }
}

macro_rules! answer {
    ($reply:ty, $value:ty, |$this:ident, $pattern:pat_param| $body:expr) => {
        impl Answer<$value> for $reply {
            fn answer(self, $pattern: $value) {
                let $this = self;
                $body;
            }
            fn fail(self, errno: i32) {
                self.error(Errno::from_i32(errno));
            }
        }
    };
}

answer! {
    ReplyEntry, (Attributes, Duration),
    |reply, (attr, ttl)| reply.entry(&ttl, &fuse_attr(&attr), GENERATION)
}
answer! { ReplyEntry, Attributes, |reply, attr| reply.entry(&TTL, &fuse_attr(&attr), GENERATION) }
answer! {
    ReplyAttr, (Attributes, Duration),
    |reply, (attr, ttl)| reply.attr(&ttl, &fuse_attr(&attr))
}
answer! { ReplyAttr, Attributes, |reply, attr| reply.attr(&TTL, &fuse_attr(&attr)) }
answer! { ReplyData, Vec<u8>, |reply, bytes| reply.data(&bytes) }
answer! { ReplyEmpty, (), |reply, ()| reply.ok() }
// Reads stay on the client path so a version switch during a read cannot expose older kernel pages.
answer! { ReplyOpen, u64, |reply, fh| reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO) }
answer! { ReplyWrite, u32, |reply, written| reply.written(written) }
answer! {
    ReplyCreate, (Attributes, u64),
    |reply, (attr, fh)| reply.created(
        &TTL,
        &fuse_attr(&attr),
        GENERATION,
        FileHandle(fh),
        FopenFlags::FOPEN_DIRECT_IO,
    )
}

fn fuse_attr(attr: &Attributes) -> FileAttr {
    let kind = match attr.kind {
        FileKind::File => fuser::FileType::RegularFile,
        FileKind::Directory => fuser::FileType::Directory,
        FileKind::Symlink => fuser::FileType::Symlink,
    };
    FileAttr {
        ino: fuser::INodeNo(attr.ino.0),
        size: attr.size,
        blocks: attr.blocks,
        atime: attr.time,
        mtime: attr.time,
        ctime: attr.time,
        crtime: attr.time,
        kind,
        perm: attr.perm,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        blksize: attr.blksize,
        flags: 0,
    }
}

pub(super) fn inode(ino: INodeNo) -> super::InodeNo {
    super::InodeNo(ino.0)
}

pub(super) fn open_mode(flags: OpenFlags) -> OpenMode {
    match (flags.acc_mode(), flags.0 & libc::O_TRUNC != 0) {
        (OpenAccMode::O_RDONLY, _) => OpenMode::Read,
        (_, true) => OpenMode::Truncate,
        _ => OpenMode::Write,
    }
}

pub(super) fn mounted(space: &Space, notifier: Notifier) {
    space.mounted(move |invalidation| match invalidation {
        Invalidation::Attributes(ino) => {
            // A negative offset invalidates attributes without touching mapped pages.
            let _ = notifier.inval_inode(INodeNo(ino.0), -1, 0);
        }
        Invalidation::Entry { parent, child, name } => {
            let _ = notifier.delete(INodeNo(parent.0), INodeNo(child.0), &name);
        }
    });
}

pub(super) fn directory(entries: Result<Vec<DirectoryEntry>>, mut reply: ReplyDirectory) {
    let entries = match entries {
        Ok(entries) => entries,
        Err(err) => return reply.error(Errno::from_i32(err.errno())),
    };
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

/// Answers an xattr read: its size when asked with size 0, ERANGE when it does not fit.
pub(super) fn xattr(value: &[u8], size: u32, reply: ReplyXattr) {
    match u32::try_from(value.len()) {
        Ok(len) if size == 0 => reply.size(len),
        Ok(len) if len <= size => reply.data(value),
        _ => reply.error(Errno::ERANGE),
    }
}

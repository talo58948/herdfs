//! How each FUSE reply carries an operation's value, or its error's errno.

use std::time::Duration;

use fuser::{
    Errno, FileAttr, FileHandle, FopenFlags, Generation, ReplyAttr, ReplyCreate, ReplyData, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite,
};

use super::TTL;

const GENERATION: Generation = Generation(0);

/// A reply that carries a `T` on success and an errno otherwise.
pub trait Answer<T> {
    fn answer(self, value: T);
    fn fail(self, errno: Errno);
}

macro_rules! answer {
    ($reply:ty, $value:ty, |$this:ident, $pattern:pat_param| $body:expr) => {
        impl Answer<$value> for $reply {
            fn answer(self, $pattern: $value) {
                let $this = self;
                $body;
            }
            fn fail(self, errno: Errno) {
                self.error(errno);
            }
        }
    };
}

answer! { ReplyEntry, (FileAttr, Duration), |reply, (attr, ttl)| reply.entry(&ttl, &attr, GENERATION) }
answer! { ReplyEntry, FileAttr, |reply, attr| reply.entry(&TTL, &attr, GENERATION) }
answer! { ReplyAttr, (FileAttr, Duration), |reply, (attr, ttl)| reply.attr(&ttl, &attr) }
answer! { ReplyAttr, FileAttr, |reply, attr| reply.attr(&TTL, &attr) }
answer! { ReplyData, Vec<u8>, |reply, bytes| reply.data(&bytes) }
answer! { ReplyEmpty, (), |reply, ()| reply.ok() }
// Reads stay on the client path so a version switch during a read cannot expose older kernel pages.
answer! { ReplyOpen, u64, |reply, fh| reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO) }
answer! { ReplyWrite, u32, |reply, written| reply.written(written) }
answer! {
    ReplyCreate, (FileAttr, u64),
    |reply, (attr, fh)| reply.created(&TTL, &attr, GENERATION, FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO)
}

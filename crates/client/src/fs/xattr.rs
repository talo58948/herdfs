use std::ffi::OsStr;

use fuser::{Errno, INodeNo, ReplyXattr};

use super::Space;

const STATS: &str = "user.herdfs.stats";
const PLAN: &str = "user.herdfs.plan";

pub fn get(space: &Space, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
    let value = match (ino, name) {
        (INodeNo::ROOT, name) if name == STATS => Some(space.stats()),
        (_, name) if name == PLAN => space.plan(space.kernel.entry(ino)),
        _ => None,
    };
    match value {
        Some(value) => answer(&value, size, reply),
        None => reply.error(Errno::NO_XATTR),
    }
}

pub fn list(space: &Space, ino: INodeNo, size: u32, reply: ReplyXattr) {
    let mut names = Vec::new();
    if ino == INodeNo::ROOT {
        names.extend_from_slice(STATS.as_bytes());
        names.push(0);
    }
    if space.plan(space.kernel.entry(ino)).is_some() {
        names.extend_from_slice(PLAN.as_bytes());
        names.push(0);
    }
    answer(&names, size, reply);
}

/// Answers with the size first and `ERANGE` when the caller's buffer is too small.
fn answer(value: &[u8], size: u32, reply: ReplyXattr) {
    match u32::try_from(value.len()) {
        Ok(len) if size == 0 => reply.size(len),
        Ok(len) if len <= size => reply.data(value),
        _ => reply.error(Errno::ERANGE),
    }
}

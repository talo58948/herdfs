use std::ffi::OsStr;

use super::{InodeNo, Space};

const STATS: &str = "user.herdfs.stats";
const PLAN: &str = "user.herdfs.plan";

pub(super) fn get(space: &Space, ino: InodeNo, name: &OsStr) -> Option<Vec<u8>> {
    match (ino, name) {
        (InodeNo::ROOT, name) if name == STATS => Some(space.stats()),
        (_, name) if name == PLAN => space.explain(space.entry_id(ino)),
        _ => None,
    }
}

pub(super) fn list(space: &Space, ino: InodeNo) -> Vec<u8> {
    let mut names = Vec::new();
    if ino == InodeNo::ROOT {
        names.extend_from_slice(STATS.as_bytes());
        names.push(0);
    }
    if space.explain(space.entry_id(ino)).is_some() {
        names.extend_from_slice(PLAN.as_bytes());
        names.push(0);
    }
    names
}

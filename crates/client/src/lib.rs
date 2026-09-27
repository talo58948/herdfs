//! The herdfs client: a FUSE mount of one space, backed by the authority.

mod cache;
mod chunk_map;
mod error;
mod fs;
mod journal;
mod mount;
mod reader;
mod remote;
mod stats;
mod strategy;

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

pub use mount::{MountArgs, mount};

/// Locks `mutex` even if a panicking request poisoned it, so one bug fails one request
/// instead of the whole mount.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

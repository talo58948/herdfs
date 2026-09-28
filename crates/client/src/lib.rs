//! The herdfs client: a FUSE mount of one space, backed by the authority.

mod cache;
mod chunk_map;
mod error;
mod fs;
mod journal;
mod link;
mod mount;
mod reader;
mod remote;
mod stats;
mod strategy;

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

pub use mount::{MountArgs, mount};
pub use strategy::{Jev, OpenContext, Strategy};

/// Locks `mutex` even if a panicking request poisoned it, so one bug fails one request
/// instead of the whole mount.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A moving average of measurements, such as download throughput or round-trip time, in which
/// each new measurement weighs a fifth.
#[derive(Default)]
struct Average(Mutex<Option<f64>>);

impl Average {
    fn add(&self, sample: f64) {
        let mut average = lock(&self.0);
        *average = Some(average.map_or(sample, |average| 0.8 * average + 0.2 * sample));
    }

    fn get(&self) -> Option<f64> {
        *lock(&self.0)
    }
}

fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

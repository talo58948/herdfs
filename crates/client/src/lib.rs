//! The herdfs client: a FUSE mount of one space, backed by the authority.

mod cache;
mod control;
mod error;
mod fs;
mod journal;
mod kept;
mod lru;
mod mount;
mod reader;
mod remote;
mod stats;
mod strategy;
mod token;
mod version;

pub mod cli;

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use error::{Error, Result};
pub use mount::{MountArgs, mount};
pub use strategy::{AccessPlan, Jev, JevAnswer, Keep, OpenContext, Prefetch, ReadAhead};

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

/// `time` in milliseconds from the epoch, negative before it, as the protocol carries it.
fn unix_ms(time: SystemTime) -> i64 {
    let ms = |span: Duration| i64::try_from(span.as_millis()).unwrap_or(i64::MAX);
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => ms(since),
        Err(before) => -ms(before.duration()),
    }
}

/// The time `ms` milliseconds from the epoch, the inverse of [`unix_ms`].
fn from_unix_ms(ms: i64) -> SystemTime {
    let span = Duration::from_millis(ms.unsigned_abs());
    let time = if ms < 0 {
        UNIX_EPOCH.checked_sub(span)
    } else {
        UNIX_EPOCH.checked_add(span)
    };
    time.unwrap_or(UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    // contract: none (docs/architecture.md: mtime is stored and reported)
    #[test]
    fn a_time_before_the_epoch_keeps_its_milliseconds() {
        let landing = UNIX_EPOCH - Duration::from_secs(14_182_980);
        assert_eq!(unix_ms(landing), -14_182_980_000);
        assert_eq!(from_unix_ms(unix_ms(landing)), landing);
    }
}

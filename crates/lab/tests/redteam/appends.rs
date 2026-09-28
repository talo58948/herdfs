//! Round 7: appends on one client against the size that client reports for the file.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;

use anyhow::Result;

use crate::two_clients;

/// Lines the logger appends, each [`LINE`] bytes long.
const LINES: usize = 3_000;
const LINE: usize = 48;
/// The logger fsyncs after this many lines, as one that flushes its buffer does.
const FSYNC_EVERY: usize = 50;

fn line(number: usize) -> Vec<u8> {
    format!("{number:06} {}\n", "x".repeat(LINE - 8)).into_bytes()
}

/// Appends [`LINES`] numbered lines to `path` through one O_APPEND descriptor, fsyncing every
/// [`FSYNC_EVERY`], while another thread opens `path` read-only, fstats it and reads its first
/// bytes in a loop, as `tail -f` does. Returns how many lines the file then lacks.
fn lines_lost_under_a_reader(path: &Path) -> Result<usize> {
    File::create(path)?;
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Relaxed) {
                if let Ok(mut file) = File::open(path) {
                    let _ = file.metadata();
                    let _ = file.read(&mut [0; 10]);
                }
            }
        });
        let appended = append_lines(path);
        done.store(true, Relaxed);
        appended
    })?;
    let kept = fs::read(path)?;
    let kept: Vec<&[u8]> = kept.split_inclusive(|&byte| byte == b'\n').collect();
    Ok((0..LINES)
        .filter(|&number| !kept.contains(&&line(number)[..]))
        .count())
}

fn append_lines(path: &Path) -> Result<()> {
    let mut log = File::options().append(true).open(path)?;
    for number in 0..LINES {
        log.write_all(&line(number))?;
        if number % FSYNC_EVERY == FSYNC_EVERY - 1 {
            log.sync_all()?;
        }
    }
    Ok(())
}

/// Repro: [`lines_lost_under_a_reader`] on a, which loses a few hundred of the 3,000 lines; on a
/// local directory it loses none, and on a without the reader none either. A getattr takes the
/// entry cached before the commit, still current by the count of settled commits (fs/mod.rs
/// `entry`), then asks the journal for local changes (fs/mod.rs `attr`) and waits on its lock,
/// which the committer holds while it drops the sealed copy and bumps that count (commit.rs
/// `finish`). The reply pairs the old head's size with no local changes, the kernel takes it for
/// the inode, and the next O_APPEND write lands at that size, over lines already fsynced.
/// Loggers, shell `>>` and git's reflogs append this way.
// contract: C1
#[test]
fn appends_after_an_fsync_never_overwrite_earlier_lines() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let lost = lines_lost_under_a_reader(&lab.mount_path("a").join("log"))?;
    assert_eq!(lost, 0, "lines of {LINES} missing from a's log");
    Ok(())
}

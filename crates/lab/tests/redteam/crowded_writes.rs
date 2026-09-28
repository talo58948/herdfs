//! Round 13: saves of one file while other processes on the same client stat it without pause.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;

use anyhow::Result;

use crate::{save, two_clients};

const SAVES: usize = 300;
/// Threads on b that keep stating the file.
const STATTERS: usize = 8;

/// Stats `path`, asking the filesystem each time rather than the kernel's cached attributes, as
/// `stat --cached=never` does.
#[expect(unsafe_code, reason = "statx with AT_STATX_FORCE_SYNC has no std wrapper")]
fn stat_uncached(path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: a zeroed `statx` is a valid value for the kernel to fill.
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is writable; both live across the call.
    let done = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_STATX_FORCE_SYNC,
            libc::STATX_BASIC_STATS,
            &mut stat,
        )
    };
    if done == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Opens `path` for writing, writes one line at its start and fsyncs it, as a program that keeps
/// a status file does.
fn save_in_place(path: &Path, line: &[u8]) -> io::Result<()> {
    let step =
        |what: &'static str| move |error: io::Error| io::Error::new(error.kind(), format!("{what}: {error}"));
    let file = OpenOptions::new().write(true).open(path).map_err(step("open"))?;
    file.write_all_at(line, 0).map_err(step("write"))?;
    file.sync_all().map_err(step("fsync"))
}

/// b saves `status`, then saves it in place 300 times, each through its own open, write and fsync;
/// with `stats`, eight other threads on b stat it without pause meanwhile. Returns the errors b's
/// saves met.
fn failed_saves(stats: bool) -> Result<Option<Vec<String>>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let path = lab.mount_path("b").join("status");
    save(&path, b"status 0\n")?;
    let stop = AtomicBool::new(false);
    let stat_until_stopped = || {
        while !stop.load(Relaxed) {
            let _ = stat_uncached(&path);
        }
    };
    let failures = thread::scope(|scope| {
        for _ in 0..if stats { STATTERS } else { 0 } {
            scope.spawn(stat_until_stopped);
        }
        let saves = (1..=SAVES).map(|save| save_in_place(&path, format!("status {save}\n").as_bytes()));
        let failures = saves
            .filter_map(Result::err)
            .map(|error| error.to_string())
            .collect();
        stop.store(true, Relaxed);
        failures
    });
    Ok(Some(failures))
}

/// Repro: [`failed_saves`] with the stats. An open makes the entry's local state and counts its
/// handle in two critical sections of the inode's lock (fs/inodes.rs `local` with `make`), and a
/// stat that takes the lock in between (fs/inodes.rs `with`, which every getattr runs) finds state
/// that counts no handle and drops it (fs/inode.rs `tidy`), so the open's handle is never counted.
/// The save's write then installs a journal copy that the same `tidy` drops at once, since nothing
/// counts a handle (journal/local.rs `idle`), and fails with EIO ("no journal copy" in the
/// client's log), leaving the copy in the journal as `<entry>.work`. The control is
/// [`saves_in_place_succeed`], without the stats.
// contract: C1, C4a (docs/architecture.md Write: a file opened for writing gets a local journal
// copy; on close the client fsyncs it and returns)
#[test]
#[ignore = "ROUND13: a stat between the two locks of an open uncounts it; its write fails with EIO"]
fn saves_in_place_succeed_while_the_file_is_stated() -> Result<()> {
    let Some(failures) = failed_saves(true)? else {
        return Ok(());
    };
    assert!(
        failures.is_empty(),
        "{} of {SAVES} saves failed: {failures:?}",
        failures.len()
    );
    Ok(())
}

// contract: C1, C4a
#[test]
fn saves_in_place_succeed() -> Result<()> {
    let Some(failures) = failed_saves(false)? else {
        return Ok(());
    };
    assert!(
        failures.is_empty(),
        "{} of {SAVES} saves failed: {failures:?}",
        failures.len()
    );
    Ok(())
}

//! Round 7: advisory locks, which coordinate the processes of one client only.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::process::Command;

use anyhow::Result;

use crate::two_clients;

/// Takes an exclusive flock(2) on `file` without waiting.
#[expect(unsafe_code, reason = "taking a lock through libc is the behavior under test")]
fn flock(file: &File) -> io::Result<()> {
    // SAFETY: flock only reads the descriptor, which `file` keeps open.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if taken == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Takes an exclusive POSIX record lock on all of `file` without waiting (F_SETLK).
#[expect(unsafe_code, reason = "taking a lock through libc is the behavior under test")]
fn record_lock(file: &File) -> io::Result<()> {
    // SAFETY: a zeroed `flock` is a valid value; the fields that matter are set below.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    // SAFETY: F_SETLK reads `lock`, which lives across the call, on a descriptor `file` holds.
    let taken = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
    if taken == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// a takes an exclusive flock(2) and an fcntl record lock (F_SETLK) on `lock`; another process
/// on a is refused both, as on a local disk. Across clients nothing is promised: the client asks
/// the kernel for neither FUSE_FLOCK_LOCKS nor FUSE_POSIX_LOCKS (fs/fuse.rs `init`), so each
/// kernel grants locks among its own processes only, and a program on another client gets them
/// too. `flock -n` guarding a cron job, or SQLite, must not be used from two clients at once.
// contract: none (docs/architecture.md: locks coordinate processes on one client, not across clients)
#[test]
fn a_lock_excludes_other_processes_on_the_same_client() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let path = lab.mount_path("a").join("lock");
    fs::write(&path, "")?;
    let held = File::options().read(true).write(true).open(&path)?;
    flock(&held)?;
    record_lock(&held)?;

    let flocked = Command::new("flock")
        .args(["-n", "-E", "75"])
        .arg(&path)
        .arg("true")
        .status()?;
    let lockf = "import fcntl, sys\nfcntl.lockf(open(sys.argv[1], 'r+'), fcntl.LOCK_EX | fcntl.LOCK_NB)";
    let record_locked = Command::new("python3").args(["-c", lockf]).arg(&path).status()?;
    assert_eq!(
        flocked.code(),
        Some(75),
        "another process's `flock -n` while a holds the lock"
    );
    assert!(
        !record_locked.success(),
        "another process's F_SETLK while a holds the lock"
    );
    Ok(())
}

//! Round 7: advisory locks taken on two clients, which v0 leaves out.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;

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

/// Whether b's attempt at a lock a holds was refused, as a lock (EWOULDBLOCK, EACCES) or as
/// out of v0 (ENOTSUP, EPERM, ENOLCK).
fn refused(taken: &io::Result<()>) -> bool {
    let errno = taken.as_ref().err().and_then(io::Error::raw_os_error);
    let refusals = [
        libc::EWOULDBLOCK,
        libc::EACCES,
        libc::ENOTSUP,
        libc::EPERM,
        libc::ENOLCK,
    ];
    errno.is_some_and(|errno| refusals.contains(&errno))
}

/// Repro: a takes an exclusive flock(2) on `lock` and holds it; b tries the same lock without
/// waiting, and gets it: both clients hold an exclusive lock on one file. The same happens with
/// fcntl record locks (F_SETLK). The client asks the kernel for neither FUSE_FLOCK_LOCKS nor
/// FUSE_POSIX_LOCKS (fs/fuse.rs `init`), so each kernel grants locks among its own processes
/// only, and says yes to every program on another client. `flock -n` guarding a cron job, and
/// SQLite's locks, then let two hosts in at once. Two processes on one client exclude each
/// other, as on a local disk; docs/architecture.md has cross-host locks fail cleanly instead.
// contract: none (docs/architecture.md: cross-host flock/fcntl locks must fail with ENOTSUP/EPERM)
#[test]
#[ignore = "REDTEAM7: flock and fcntl locks succeed on two clients at once instead of failing cleanly"]
fn a_lock_held_on_one_client_is_refused_on_another() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    fs::write(lab.mount_path("a").join("lock"), "")?;
    lab.wait_committed("a")?;
    let open = |client| {
        let path = lab.mount_path(client).join("lock");
        File::options().read(true).write(true).open(path)
    };
    let held = open("a")?;
    flock(&held)?;
    record_lock(&held)?;

    let other = open("b")?;
    let (flocked, record_locked) = (flock(&other), record_lock(&other));
    assert!(
        refused(&flocked) && refused(&record_locked),
        "b's flock gave {flocked:?} and its F_SETLK {record_locked:?} while a holds both"
    );
    Ok(())
}

//! Round 32: swapping an agents session's checked working directory.
#![allow(
    unsafe_code,
    clippy::unwrap_used,
    reason = "test-only atomic directory exchange and assertions"
)]

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use tempfile::TempDir;

use crate::TestChild;

const TRIALS: usize = 40;

fn start_runner(mount: &Path) -> Result<TestChild> {
    let mut command = TestChild::agents()?;
    command
        .arg("--mount")
        .arg(mount)
        .args(["--host", "box", "--command", "pwd -P"])
        .args(["--poll-ms", "5", "--max-sessions", "40"]);
    TestChild::spawn(command)
}

fn session(host: &Path, name: &str, cwd: &str) -> Result<PathBuf> {
    let dir = host.join(name);
    fs::create_dir(&dir)?;
    fs::write(dir.join("prompt"), "run\n")?;
    fs::write(dir.join("cwd"), format!("{cwd}\n"))?;
    Ok(dir)
}

#[cfg(target_os = "linux")]
fn exchange(left: &Path, right: &Path) -> Result<()> {
    let left = CString::new(left.as_os_str().as_bytes())?;
    let right = CString::new(right.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    ensure!(result == 0, "renameat2: {}", std::io::Error::last_os_error());
    Ok(())
}

#[cfg(target_os = "macos")]
fn exchange(left: &Path, right: &Path) -> Result<()> {
    let left = CString::new(left.as_os_str().as_bytes())?;
    let right = CString::new(right.as_os_str().as_bytes())?;
    // SAFETY: both paths are NUL-terminated and remain alive for the call.
    let result = unsafe { libc::renameatx_np(libc::AT_FDCWD, left.as_ptr(), libc::AT_FDCWD, right.as_ptr(), libc::RENAME_SWAP) };
    ensure!(result == 0, "renameatx_np: {}", std::io::Error::last_os_error());
    Ok(())
}

fn wait_finished(sessions: &[PathBuf]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while sessions.iter().any(|dir| {
        fs::read_to_string(dir.join("status"))
            .map(|status| matches!(status.trim(), "queued" | "running"))
            .unwrap_or(true)
    }) {
        ensure!(Instant::now() < deadline, "sessions did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// A stable in-mount cwd is the passing control. Swapping another checked cwd with an outside
/// symlink must never let the command start outside the public mount.
// contract: A1
#[test]
fn a_checked_session_cwd_cannot_be_swapped_outside_the_mount() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let mount = TempDir::new()?;
    let outside = TempDir::new()?;
    let mount = mount.path().canonicalize()?;
    let outside = outside.path().canonicalize()?;
    let host = mount.join(".agents/box");
    fs::create_dir_all(&host)?;
    fs::create_dir(mount.join("control-work"))?;
    fs::create_dir(mount.join("work"))?;
    symlink(&outside, mount.join("escape"))?;
    let control = session(&host, "control", "control-work")?;
    let sessions: Vec<_> = (0..TRIALS)
        .map(|trial| session(&host, &format!("attack-{trial}"), "work"))
        .collect::<Result<_>>()?;

    let running = Arc::new(AtomicBool::new(true));
    let swapping = Arc::clone(&running);
    let left = mount.join("work");
    let right = mount.join("escape");
    let swapper = thread::spawn(move || {
        while swapping.load(Ordering::Relaxed) {
            exchange(&left, &right).unwrap();
        }
    });
    let _runner = start_runner(&mount)?;
    wait_finished(&sessions)?;
    wait_finished(std::slice::from_ref(&control))?;
    running.store(false, Ordering::Relaxed);
    swapper.join().unwrap();

    assert_eq!(fs::read_to_string(control.join("status"))?.trim(), "done");
    assert_eq!(
        fs::read_to_string(control.join("output"))?.trim(),
        mount.join("control-work").to_str().unwrap()
    );
    let escaped = sessions
        .iter()
        .filter(|dir| {
            fs::read_to_string(dir.join("output")).is_ok_and(|text| text.trim() == outside.to_str().unwrap())
        })
        .count();
    assert_eq!(
        escaped, 0,
        "{escaped}/{TRIALS} commands started outside the mount"
    );
    Ok(())
}

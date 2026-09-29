//! Round 33: an offline truncating open through a held directory after another client moves it.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::thread;

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, Links};

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

const TRIALS: usize = 3;

/// Lists a child through b's held directory, optionally lets a move the directory, then replaces
/// the child's bytes through that descriptor while b is offline. Returns the write result and
/// the bytes at the child's authoritative path after reconnect.
fn replace_below_held(lab: &Lab, trial: usize, moved: bool) -> Result<(std::io::Result<()>, Vec<u8>)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let old = format!("held-dir-{moved}-{trial}");
    let new = format!("moved-dir-{moved}-{trial}");
    fs::create_dir(a.join(&old))?;
    save(&a.join(&old).join("child"), b"old")?;
    thread::sleep(VISIBLE_AFTER);

    let held = File::open(b.join(&old))?;
    let through_fd = PathBuf::from(format!("/proc/self/fd/{}", held.as_raw_fd()));
    ensure!(fs::read_dir(&through_fd)?.count() == 1, "b lists the child");
    let authoritative = if moved {
        fs::rename(a.join(&old), a.join(&new))?;
        a.join(&new).join("child")
    } else {
        a.join(&old).join("child")
    };
    thread::sleep(VISIBLE_AFTER);
    held.metadata()?;

    lab.cut("b", Links::Authority)?;
    let written = fs::write(through_fd.join("child"), b"new");
    lab.restore("b", Links::Authority)?;
    if written.is_ok() {
        lab.wait_committed("b")?;
        thread::sleep(VISIBLE_AFTER);
    }
    Ok((written, fs::read(authoritative)?))
}

/// A client that holds a directory and has listed its child can replace that child's contents
/// offline after another client moves the directory. Keeping the directory fixed is the control.
// contract: C7, C12
#[test]
#[ignore = "ROUND33: moving a held directory strands its cached children for offline writes"]
fn an_offline_replacement_below_a_relocated_held_directory_lands() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (written, bytes) = replace_below_held(&lab, 0, false)?;
    ensure!(written.is_ok(), "control offline replacement: {written:?}");
    assert_bytes(&bytes, b"new", "control bytes");

    let mut failures = 0;
    for trial in 0..TRIALS {
        let (written, bytes) = replace_below_held(&lab, trial, true)?;
        eprintln!("trial {trial}: replacement={written:?}, bytes={bytes:?}");
        failures += usize::from(written.is_err() || bytes != b"new");
    }
    assert_eq!(failures, 0, "offline replacements that failed, of {TRIALS}");
    Ok(())
}

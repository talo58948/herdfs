//! Round 31: reopening a held entry offline after its old path was reused.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::thread;

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, Links};

use crate::{VISIBLE_AFTER, assert_bytes, read_at, save, two_clients};

const OLD: &[u8] = b"old-data";
const CHANGED: &[u8] = b"changed!";
const REPLACEMENT: &[u8] = b"replacement";
const TRIALS: usize = 3;

/// Holds the original file on b, optionally moves it and reuses its old path on a, then reopens
/// the held inode through `/proc/self/fd` for an offline write. Returns the write result and the
/// bytes at the original entry's authoritative path after reconnect.
fn write_held_offline(lab: &Lab, trial: usize, moved: bool) -> Result<(std::io::Result<()>, Vec<u8>)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let original = format!("held-{moved}-{trial}");
    let relocated = format!("relocated-{moved}-{trial}");
    save(&a.join(&original), OLD)?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join(&original))?;
    assert_bytes(&read_at(&held, 0, OLD.len())?, OLD, "b's held current bytes");

    let authoritative = if moved {
        fs::rename(a.join(&original), a.join(&relocated))?;
        save(&a.join(&original), REPLACEMENT)?;
        thread::sleep(VISIBLE_AFTER);
        assert_bytes(&fs::read(b.join(&original))?, REPLACEMENT, "b's replacement");
        a.join(&relocated)
    } else {
        a.join(&original)
    };

    lab.cut("b", Links::Authority)?;
    let through_fd = format!("/proc/self/fd/{}", held.as_raw_fd());
    let written = OpenOptions::new()
        .read(true)
        .write(true)
        .open(through_fd)
        .and_then(|file| file.write_all_at(CHANGED, 0));
    lab.restore("b", Links::Authority)?;
    if written.is_ok() {
        lab.wait_committed("b")?;
        thread::sleep(VISIBLE_AFTER);
    }
    Ok((written, fs::read(authoritative)?))
}

/// An offline content write through a held entry follows its rename even after the old path is
/// reused. The same reopen without a rename is the control.
// contract: C7, C12
#[test]
fn an_offline_write_through_a_held_relocated_entry_is_journaled() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (written, bytes) = write_held_offline(&lab, 0, false)?;
    ensure!(written.is_ok(), "control offline reopen: {written:?}");
    assert_bytes(&bytes, CHANGED, "control bytes");

    let mut failures = 0;
    for trial in 0..TRIALS {
        let (written, bytes) = write_held_offline(&lab, trial, true)?;
        eprintln!("trial {trial}: reopen={written:?}, bytes={bytes:?}");
        failures += usize::from(written.is_err() || bytes != CHANGED);
    }
    assert_eq!(failures, 0, "offline held-entry writes that failed, of {TRIALS}");
    Ok(())
}

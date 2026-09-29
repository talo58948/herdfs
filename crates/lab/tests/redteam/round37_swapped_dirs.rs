//! Round 37: two held directories exchanging paths after both subtrees are known.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::thread;

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, Links};

use crate::{VISIBLE_AFTER, save, two_clients};

const TRIALS: usize = 3;

fn listed(path: &PathBuf) -> Result<()> {
    ensure!(
        fs::read_dir(path)?.count() == 1,
        "{} has one child",
        path.display()
    );
    Ok(())
}

/// Holds and lists two directories, optionally exchanges their paths through a temporary name,
/// then refreshes both held inodes in a fixed order. Truncating offline writes need no old file
/// bytes, so no authority request can repair either known subtree.
fn swap_and_write(
    lab: &Lab,
    trial: usize,
    swapped: bool,
) -> Result<(std::io::Result<()>, std::io::Result<()>, bool)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let left = format!("swap-{swapped}-{trial}-left");
    let right = format!("swap-{swapped}-{trial}-right");
    let temporary = format!("swap-{swapped}-{trial}-temporary");
    fs::create_dir(a.join(&left))?;
    fs::create_dir(a.join(&right))?;
    save(&a.join(&left).join("left-child"), b"left-old")?;
    save(&a.join(&right).join("right-child"), b"right-old")?;
    thread::sleep(VISIBLE_AFTER);

    let left_held = File::open(b.join(&left))?;
    let right_held = File::open(b.join(&right))?;
    let left_fd = PathBuf::from(format!("/proc/self/fd/{}", left_held.as_raw_fd()));
    let right_fd = PathBuf::from(format!("/proc/self/fd/{}", right_held.as_raw_fd()));
    listed(&left_fd)?;
    listed(&right_fd)?;

    if swapped {
        fs::rename(a.join(&left), a.join(&temporary))?;
        fs::rename(a.join(&right), a.join(&left))?;
        fs::rename(a.join(&temporary), a.join(&right))?;
        thread::sleep(VISIBLE_AFTER);
    }
    left_held.metadata()?;
    right_held.metadata()?;

    lab.cut("b", Links::Authority)?;
    let left_write = fs::write(left_fd.join("left-child"), b"left-written");
    let right_write = fs::write(right_fd.join("right-child"), b"right-written");
    lab.restore("b", Links::Authority)?;
    if left_write.is_ok() || right_write.is_ok() {
        lab.wait_committed("b")?;
        thread::sleep(VISIBLE_AFTER);
    }
    let paths = if swapped { (&right, &left) } else { (&left, &right) };
    let landed = fs::read(a.join(paths.0).join("left-child"))? == b"left-written"
        && fs::read(a.join(paths.1).join("right-child"))? == b"right-written";
    Ok((left_write, right_write, landed))
}

/// A stationary pair controls three pairs that exchange paths after both subtrees are listed.
/// Refreshing each held inode must carry each known subtree to the entry's new path.
// contract: C7, C12
#[test]
fn both_known_subtrees_follow_exchanged_directory_paths() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (left, right, landed) = swap_and_write(&lab, 0, false)?;
    left?;
    right?;
    ensure!(landed, "stationary control writes land");

    let mut failures = 0;
    for trial in 0..TRIALS {
        let (left, right, landed) = swap_and_write(&lab, trial, true)?;
        eprintln!("trial {trial}: left={left:?}, right={right:?}, landed={landed}");
        let correct = left.is_ok() && right.is_ok() && landed;
        failures += usize::from(!correct);
    }
    assert_eq!(failures, 0, "swapped pairs with a lost subtree, of {TRIALS}");
    Ok(())
}

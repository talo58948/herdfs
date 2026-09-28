//! Round 21: a path-based mutation racing a rename of the directory inode it is relative to.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

use crate::VISIBLE_AFTER;

const SLOW_AUTHORITY: Profile = Profile::Custom {
    round_trip_ms: 2_000,
    kilobytes_per_sec: 100_000,
    store_first_byte_ms: 0,
};
const RENAME_AFTER: Duration = Duration::from_millis(6_400);

fn start() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], SLOW_AUTHORITY, Strategy::Stream);
    Lab::start(config).map(Some)
}

/// Makes `child` relative to a directory descriptor on b. With `rename`, a moves that directory
/// after b's Locate reply but before its Mkdir request reaches the authority.
fn make_child(lab: &Lab, trial: usize, rename: bool) -> Result<(std::io::Result<()>, bool)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let old = format!("d-{rename}-{trial}");
    let new = format!("e-{rename}-{trial}");
    fs::create_dir(a.join(&old))?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join(&old))?;
    thread::sleep(VISIBLE_AFTER);
    let child = PathBuf::from(format!("/proc/self/fd/{}/child", held.as_raw_fd()));
    let made = thread::scope(|scope| {
        let mkdir = scope.spawn(|| fs::create_dir(child));
        if rename {
            thread::sleep(RENAME_AFTER);
            fs::rename(a.join(&old), a.join(&new))?;
        }
        mkdir.join().map_err(|_| anyhow!("mkdir thread panicked"))
    })?;
    let parent = if rename { new } else { old };
    Ok((made, a.join(parent).join("child").is_dir()))
}

/// Repro: b holds a directory inode whose path is older than T_vis and makes a child through its
/// descriptor. Its slow Locate reaches the authority before a renames the directory, but its
/// subsequent path-only Mkdir reaches it after the rename. The mutation must follow the held
/// entry; the controls leave the directory in place.
// contract: C7
#[test]
fn a_mutation_follows_a_held_directory_when_its_locate_races_a_rename() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let (made, landed) = make_child(&lab, 0, false)?;
    assert!(made.is_ok() && landed, "control: {made:?}, landed={landed}");
    let mut missed = 0;
    for trial in 0..3 {
        let (made, landed) = make_child(&lab, trial, true)?;
        missed += usize::from(made.is_err() || !landed);
    }
    assert_eq!(missed, 0, "mutations that missed the held directory, of 3");
    Ok(())
}

//! Round 23: a path-based mutation racing a rename and reuse of its held parent directory's path.

use std::fs::{self, File};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, mkdir_entry};

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

/// Makes a child relative to b's held directory. In the attack, a moves that directory after b's
/// Locate reply, then immediately reuses its old path before b's Mkdir reaches the authority.
fn make_child(lab: &Lab, trial: usize, reuse: bool) -> Result<[bool; 2]> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let old = format!("old-{reuse}-{trial}");
    let new = format!("new-{reuse}-{trial}");
    fs::create_dir(a.join(&old))?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join(&old))?;
    thread::sleep(VISIBLE_AFTER);
    thread::scope(|scope| {
        let mkdir = scope.spawn(|| mkdir_entry(&held, "child"));
        if reuse {
            thread::sleep(RENAME_AFTER);
            fs::rename(a.join(&old), a.join(&new))?;
            fs::create_dir(a.join(&old))?;
        }
        mkdir.join().map_err(|_| anyhow!("mkdir thread panicked"))??;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok([
        a.join(old).join("child").is_dir(),
        a.join(new).join("child").is_dir(),
    ])
}

/// The mutation must follow the directory entry b holds even when its former path is reused. The
/// control leaves the parent in place; three attacks move it and install a replacement there.
// contract: C7
#[test]
fn a_mutation_racing_a_rename_does_not_land_in_a_reused_parent_path() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    assert_eq!(make_child(&lab, 0, false)?, [true, false], "control");
    let mut misplaced = 0;
    for trial in 0..3 {
        let landed = make_child(&lab, trial, true)?;
        eprintln!("trial {trial}: old={}, new={}", landed[0], landed[1]);
        misplaced += usize::from(landed != [false, true]);
    }
    assert_eq!(misplaced, 0, "mutations misplaced after the parent moved, of 3");
    Ok(())
}

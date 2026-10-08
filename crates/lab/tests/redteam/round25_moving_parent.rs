//! Round 25: a namespace call while its held parent keeps moving to fresh paths.

use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, mkdir_entry};

const SLOW_AUTHORITY: Profile = Profile::Custom {
    round_trip_ms: 2_000,
    kilobytes_per_sec: 100_000,
    store_first_byte_ms: 0,
};
const MOVE_EVERY: Duration = Duration::from_millis(200);
const TRIALS: usize = 3;

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

/// Makes a child through b's held directory descriptor while a optionally keeps renaming that
/// directory. Returns the syscall result, the last path, and how many times the parent moved.
fn make_child(lab: &Lab, trial: usize, moving: bool) -> Result<(std::io::Result<()>, PathBuf, usize)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let first = format!("moving-{moving}-{trial}-0");
    fs::create_dir(a.join(&first))?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join(&first))?;
    thread::sleep(VISIBLE_AFTER);
    let stop = AtomicBool::new(false);
    let (made, last, moves) = thread::scope(|scope| {
        let mkdir = scope.spawn(|| mkdir_entry(&held, "child"));
        let rename = scope.spawn(|| -> Result<(PathBuf, usize)> {
            let mut current = a.join(first);
            let mut moves = 0;
            while moving && !stop.load(Relaxed) {
                let next = a.join(format!("moving-{moving}-{trial}-{}", moves + 1));
                fs::rename(&current, &next)?;
                current = next;
                moves += 1;
                thread::sleep(MOVE_EVERY);
            }
            Ok((current, moves))
        });
        let made = mkdir.join().map_err(|_| anyhow!("mkdir thread panicked"))?;
        stop.store(true, Relaxed);
        let (last, moves) = rename.join().map_err(|_| anyhow!("rename thread panicked"))??;
        Ok::<_, anyhow::Error>((made, last, moves))
    })?;
    Ok((made, last, moves))
}

/// A namespace operation relative to a held directory follows that entry even when it moves more
/// than twice before the operation can reach the authority. The control leaves its parent fixed.
// contract: C7
#[test]
fn a_namespace_call_follows_a_parent_that_moves_repeatedly() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let (made, parent, moves) = make_child(&lab, 0, false)?;
    assert!(made.is_ok() && parent.join("child").is_dir(), "control: {made:?}");
    assert_eq!(moves, 0, "control parent moves");

    let mut failures = 0;
    for trial in 0..TRIALS {
        let (made, parent, moves) = make_child(&lab, trial, true)?;
        ensure!(moves > 2, "trial {trial} moved only {moves} times");
        let landed = parent.join("child").is_dir();
        eprintln!("trial {trial}: {moves} moves, mkdir={made:?}, landed={landed}");
        failures += usize::from(made.is_err() || !landed);
    }
    assert_eq!(
        failures, 0,
        "namespace calls that missed the held parent, of {TRIALS}"
    );
    Ok(())
}

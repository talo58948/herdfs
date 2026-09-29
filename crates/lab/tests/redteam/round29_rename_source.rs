//! Round 29: a rename after another client replaces the entry at its cached source path.

use std::fs;
use std::thread;

use anyhow::Result;

use crate::{VISIBLE_AFTER, save, two_clients};

const TRIALS: usize = 3;

/// Caches both entries on b, optionally swaps the second into the first's path on a, then has b
/// rename that path. Returns what b reads at the destination and what the authority exposes there.
fn rename_reused_source(trial: usize, reuse: bool) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let source = format!("source-{reuse}-{trial}");
    let incoming = format!("incoming-{reuse}-{trial}");
    let old = format!("old-{reuse}-{trial}");
    let destination = format!("destination-{reuse}-{trial}");
    let source_bytes = format!("source {trial}");
    let incoming_bytes = format!("incoming {trial}");
    save(&a.join(&source), source_bytes.as_bytes())?;
    save(&a.join(&incoming), incoming_bytes.as_bytes())?;
    thread::sleep(VISIBLE_AFTER);
    assert_eq!(fs::read(b.join(&source))?, source_bytes.as_bytes());
    assert_eq!(fs::read(b.join(&incoming))?, incoming_bytes.as_bytes());

    if reuse {
        fs::rename(a.join(&source), a.join(&old))?;
        fs::rename(a.join(&incoming), a.join(&source))?;
    }
    fs::rename(b.join(&source), b.join(&destination))?;
    let local = fs::read(b.join(&destination))?;
    thread::sleep(VISIBLE_AFTER);
    let authoritative = fs::read(a.join(&destination))?;
    Ok(Some((local, authoritative)))
}

/// A rename must leave its own client bound to the entry that the authority resolved and moved.
/// The control leaves the source alone; three attacks replace it after b caches both entries.
// contract: C7 (rename is atomic; operations after a rename reach the moved entry)
#[test]
fn rename_of_a_reused_source_keeps_the_authoritys_entry_at_the_destination() -> Result<()> {
    let Some(control) = rename_reused_source(0, false)? else {
        return Ok(());
    };
    assert_eq!(control.0, control.1, "control");

    let mut diverged = 0;
    for trial in 0..TRIALS {
        let Some((local, authoritative)) = rename_reused_source(trial, true)? else {
            return Ok(());
        };
        eprintln!(
            "trial {trial}: local={:?}, authority={:?}",
            String::from_utf8_lossy(&local),
            String::from_utf8_lossy(&authoritative)
        );
        diverged += usize::from(local != authoritative);
    }
    assert_eq!(diverged, 0, "divergent renames, of {TRIALS}");
    Ok(())
}

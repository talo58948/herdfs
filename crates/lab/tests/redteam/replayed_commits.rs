//! Round 9: a client killed after the authority committed its write but before the answer
//! arrived, whose journal sends that commit again after restart.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use herdfs_lab::{ADMIN, Collection, Lab, LabConfig, Profile, Strategy, run};

use crate::{SLOW_ANSWERS, journaled, random_bytes, replay_history, save, wait_for_rewrite};

const LEN: usize = 1 << 20;
const PRUNED_WITHIN: Duration = Duration::from_secs(10);

/// a commits v1 of `f`, then closes a rewrite of it without fsync. The moment the authority
/// has committed the rewrite, and before its answer crosses a's slow link, the test stops a's
/// mount process and SIGKILLs it: the journal still holds the write. b then saves `f` three
/// times, the test waits until the authority prunes the rewrite if `collection` makes it, and
/// a restarts, which sends the journaled commit again under its request id. Returns which write
/// the head holds once a has nothing left to commit: b's last save, or a's rewrite again.
fn replay_after_three_saves(collection: Option<Collection>) -> Result<Option<&'static str>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], SLOW_ANSWERS, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream);
    let config = match collection {
        Some(settings) => config.collection(settings),
        None => config,
    };
    let mut lab = Lab::start(config)?;
    save(&lab.mount_path("a").join("f"), &random_bytes(LEN))?;
    let v1 = lab.head(ADMIN, "f")?;
    fs::write(lab.mount_path("a").join("f"), random_bytes(LEN))?;
    let rewrite = wait_for_rewrite(&lab, "f", &v1)?;
    let pid = lab.client_pid("a").context("a runs")?;
    run(Command::new("kill").args(["-STOP", &pid.to_string()]))?;
    lab.kill_client("a")?;
    ensure!(journaled(&lab)?, "a settled the rewrite before it was stopped");

    for _ in 0..3 {
        save(&lab.mount_path("b").join("f"), &random_bytes(LEN))?;
    }
    let last = lab.head(ADMIN, "f")?;
    let deadline = Instant::now() + PRUNED_WITHIN;
    while collection.is_some() && replay_history(&lab, "f")?.contains(&rewrite) {
        if Instant::now() > deadline {
            bail!("the rewrite is kept after {PRUNED_WITHIN:?}");
        }
        thread::sleep(Duration::from_millis(100));
    }
    lab.restart_client("a")?;
    lab.wait_committed("a")?;
    let head = lab.head(ADMIN, "f")?.chunks;
    let holds = match head {
        head if head == last.chunks => "b's last save",
        head if head == rewrite.chunks => "a's rewrite",
        _ => "neither",
    };
    Ok(Some(holds))
}

/// Repro: [`replay_after_three_saves`] with `Collection::QUICK`, which keeps 3 versions, so the
/// rewrite is pruned before a restarts, and the authority's record of its request id goes with
/// it (the `commits` row cascades). The replayed commit names v1 as its base, also pruned, gets
/// `Conflict`, and the committer sends it once more without a base: the rewrite that the
/// authority acknowledged before b's three saves becomes the head again, over b's last save.
/// [`a_replayed_commit_of_a_retained_version_changes_nothing`] is the control.
// contract: C5, C4a
#[test]
fn a_replayed_commit_of_a_pruned_version_changes_nothing() -> Result<()> {
    let Some(head) = replay_after_three_saves(Some(Collection::QUICK))? else {
        return Ok(());
    };
    assert_eq!(head, "b's last save", "the head after a's restart");
    Ok(())
}

// contract: C5, C4a
#[test]
fn a_replayed_commit_of_a_retained_version_changes_nothing() -> Result<()> {
    let Some(head) = replay_after_three_saves(None)? else {
        return Ok(());
    };
    assert_eq!(head, "b's last save", "the head after a's restart");
    Ok(())
}

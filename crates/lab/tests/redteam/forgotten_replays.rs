//! Round 17: a client killed after the authority committed its write but before the answer
//! arrived, and restarted only after collection has dropped that commit's upload records and
//! request id.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use herdfs_lab::{ADMIN, Collection, Lab, LabConfig, Profile, Strategy, run};

use crate::{SLOW_ANSWERS, journaled, random_bytes, replay_history, save, wait_for_rewrite};

const LEN: usize = 1 << 20;
/// Under `Collection::QUICK` (grace 5 s, collection every second, 3 versions kept), a commits
/// v1 of `f`, then closes a rewrite without fsync. The moment the authority has committed the
/// rewrite, and before its answer crosses a's slow link, the test stops and SIGKILLs a's mount:
/// the journal still holds the write. b saves `f` once, which keeps the rewrite and v1 in
/// retention. a stays down for `down`, then restarts and sends the journaled commit again under
/// its request id. Returns which write the head holds once a has nothing left to commit, and how
/// many versions hold the rewrite's bytes.
fn replay_after(down: Duration) -> Result<Option<(&'static str, usize)>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], SLOW_ANSWERS, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
        .collection(Collection::QUICK);
    let mut lab = Lab::start(config)?;
    save(&lab.mount_path("a").join("f"), &random_bytes(LEN))?;
    let v1 = lab.head(ADMIN, "f")?;
    fs::write(lab.mount_path("a").join("f"), random_bytes(LEN))?;
    let rewrite = wait_for_rewrite(&lab, "f", &v1)?;
    let pid = lab.client_pid("a").context("a runs")?;
    run(Command::new("kill").args(["-STOP", &pid.to_string()]))?;
    lab.kill_client("a")?;
    ensure!(journaled(&lab)?, "a settled the rewrite before it was stopped");

    save(&lab.mount_path("b").join("f"), &random_bytes(LEN))?;
    let last = lab.head(ADMIN, "f")?;
    thread::sleep(down);
    ensure!(
        replay_history(&lab, "f")?.contains(&rewrite),
        "retention kept the rewrite while a was down"
    );
    lab.restart_client("a")?;
    lab.wait_committed("a")?;
    let head = lab.head(ADMIN, "f")?.chunks;
    let holds = match head {
        head if head == last.chunks => "b's save",
        head if head == rewrite.chunks => "a's rewrite",
        _ => "neither",
    };
    let copies = replay_history(&lab, "f")?
        .iter()
        .filter(|version| version.chunks == rewrite.chunks)
        .count();
    Ok(Some((holds, copies)))
}

/// [`replay_after`] with a down for 10 s, twice the grace. Collection used to delete the
/// `commits` row of the rewrite's request id once it was older than retain-days plus the grace,
/// so the authority no longer knew the replay; its base v1 was still retained, so it committed
/// the rewrite a second time, over b's save. Request ids are now kept at least 30 days
/// (collect.rs `REQUEST_MEMORY_MS`). The control is
/// [`a_replay_within_the_grace_commits_nothing_new`].
// contract: C5, C4a
#[test]
fn a_replay_after_the_grace_commits_nothing_new() -> Result<()> {
    let Some(outcome) = replay_after(Duration::from_secs(10))? else {
        return Ok(());
    };
    assert_eq!(
        outcome,
        ("b's save", 1),
        "the head, and versions holding the rewrite"
    );
    Ok(())
}

/// Control: the same with a down for one second, within the grace.
// contract: C5, C4a
#[test]
fn a_replay_within_the_grace_commits_nothing_new() -> Result<()> {
    let Some(outcome) = replay_after(Duration::from_secs(1))? else {
        return Ok(());
    };
    assert_eq!(
        outcome,
        ("b's save", 1),
        "the head, and versions holding the rewrite"
    );
    Ok(())
}

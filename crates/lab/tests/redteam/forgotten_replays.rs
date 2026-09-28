//! Round 17: a client killed after the authority committed its write but before the answer
//! arrived, and restarted only after collection has dropped that commit's upload records and
//! request id.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Request, Response, Version};
use herdfs_lab::{ADMIN, Collection, Lab, LabConfig, Profile, SPACE, Strategy, run};

use crate::{random_bytes, save};

const LEN: usize = 1 << 20;
/// a's links: half a second each way, so the answer to a commit is still on its way when the
/// test sees the version at the authority and stops a.
const SLOW_ANSWERS: Profile = Profile::Custom {
    round_trip_ms: 1_000,
    kilobytes_per_sec: 100_000,
    store_first_byte_ms: 0,
};
const COMMITTED_WITHIN: Duration = Duration::from_secs(30);

/// The versions of `f` in [`SPACE`], as the admin sees them.
fn history(lab: &Lab) -> Result<Vec<Version>> {
    let request = Request::History {
        space: SPACE.into(),
        path: SpacePath::try_from("f")?,
    };
    match lab.rpc(ADMIN, &request)? {
        Response::History { versions } => Ok(versions),
        reply => bail!("history answers with versions, not {reply:?}"),
    }
}

/// Polls the authority until a version of `f` other than `known` appears, and returns it.
fn wait_for_new_version(lab: &Lab, known: &Version) -> Result<Version> {
    let deadline = Instant::now() + COMMITTED_WITHIN;
    loop {
        if let Some(new) = history(lab)?.into_iter().find(|version| version != known) {
            return Ok(new);
        }
        if Instant::now() > deadline {
            bail!("no commit after {COMMITTED_WITHIN:?}");
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// Whether a's journal holds a record of a write still to commit.
fn journaled(lab: &Lab) -> Result<bool> {
    let records = fs::read_dir(lab.client_dir("a").join("journal"))?;
    for record in records {
        let path = record?.path();
        if path.extension().is_some_and(|extension| extension == "json") {
            return Ok(true);
        }
    }
    Ok(false)
}

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
    let rewrite = wait_for_new_version(&lab, &v1)?;
    let pid = lab.client_pid("a").context("a runs")?;
    run(Command::new("kill").args(["-STOP", &pid.to_string()]))?;
    lab.kill_client("a")?;
    ensure!(journaled(&lab)?, "a settled the rewrite before it was stopped");

    save(&lab.mount_path("b").join("f"), &random_bytes(LEN))?;
    let last = lab.head(ADMIN, "f")?;
    thread::sleep(down);
    ensure!(
        history(&lab)?.contains(&rewrite),
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
    let copies = history(&lab)?
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

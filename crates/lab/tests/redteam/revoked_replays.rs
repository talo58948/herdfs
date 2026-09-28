//! Round 15: journal retries after a commit landed but its reply was lost and access changed.

use std::fs::{self, File};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request, Response, Version};
use herdfs_lab::{ADMIN, Lab, LabConfig, Links, Profile, SPACE, Strategy};

use crate::save;

const TRIALS: usize = 3;
const COMMITTED_WITHIN: Duration = Duration::from_secs(30);
const SLOW_ANSWERS: Profile = Profile::Custom {
    round_trip_ms: 1_000,
    kilobytes_per_sec: 100_000,
    store_first_byte_ms: 0,
};

fn history(lab: &Lab, path: &str) -> Result<Vec<Version>> {
    let request = Request::History {
        space: SPACE.into(),
        path: SpacePath::try_from(path)?,
    };
    match lab.rpc(ADMIN, &request)? {
        Response::History { versions } => Ok(versions),
        reply => bail!("history answers with versions, not {reply:?}"),
    }
}

fn wait_for_rewrite(lab: &Lab, path: &str, first: &Version) -> Result<Version> {
    let deadline = Instant::now() + COMMITTED_WITHIN;
    loop {
        if let Some(version) = history(lab, path)?.into_iter().find(|version| version != first) {
            return Ok(version);
        }
        ensure!(Instant::now() < deadline, "no commit after {COMMITTED_WITHIN:?}");
        thread::sleep(Duration::from_millis(2));
    }
}

fn set_alice(lab: &Lab, perm: Option<Perm>) -> Result<()> {
    let request = Request::SetAcl {
        space: SPACE.into(),
        path: SpacePath::root(),
        principal: "user:alice".into(),
        perm,
    };
    lab.rpc(ADMIN, &request)?;
    Ok(())
}

/// Repro: alice closes a rewrite and opens the file while it is pending. The authority commits
/// the rewrite, but its reply is still crossing alice's slow link when that link is cut. Access is
/// then revoked before the journal retries. The replay must confirm the retained commit so the
/// client drops its journal copy and `fsync` succeeds. Three revoked trials run beside a control.
// contract: C4a, S8 (docs/architecture.md: fsync returns EIO if the commit fails)
#[test]
fn fsync_accepts_a_commit_replayed_after_revocation() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], SLOW_ANSWERS, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream);
    let lab = Lab::start(config)?;
    let mut outcomes = Vec::new();

    for trial in 0..=TRIALS {
        let name = format!("replay-{trial}");
        let path = lab.mount_path("a").join(&name);
        save(&path, b"first")?;
        let first = lab.head(ADMIN, &name)?;
        let rewrite = format!("rewrite-{trial}");
        fs::write(&path, rewrite.as_bytes())?;
        let held = File::open(&path)?;
        let committed = wait_for_rewrite(&lab, &name, &first)?;

        lab.cut("a", Links::Authority)?;
        thread::sleep(Duration::from_millis(600));
        ensure!(
            lab.stats("a")?["pending_commits"] == 1,
            "trial {trial}: alice settled before its commit answer was cut"
        );
        let revoked = trial < TRIALS;
        if revoked {
            set_alice(&lab, Some(Perm::Read))?;
        }
        lab.restore("a", Links::Authority)?;
        lab.wait_committed("a")?;

        let synced = held.sync_all();
        ensure!(
            lab.head(ADMIN, &name)?.id == committed.id,
            "trial {trial}: the acknowledged rewrite is still the head"
        );
        outcomes.push((revoked, synced));
        if revoked {
            set_alice(&lab, Some(Perm::Write))?;
        }
    }
    let control = &outcomes[TRIALS].1;
    ensure!(control.is_ok(), "control without revocation: {control:?}");
    let refused = outcomes[..TRIALS]
        .iter()
        .filter(|(_, synced)| synced.is_err())
        .count();
    assert_eq!(
        refused, 0,
        "fsync rejected {refused}/{TRIALS} acknowledged commits after their replay was denied"
    );
    Ok(())
}

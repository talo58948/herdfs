//! Round 100: bounded replay memory must not stop fresh NFS mutations.

use std::time::Duration;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::{Rpc, opaque, status};

const REPLAY_LIMIT: usize = 4_096;

fn entry(lab: &Lab, name: &str) -> Result<Entry> {
    let response = lab.rpc(
        "alice",
        &Request::Stat {
            space: SPACE.into(),
            path: SpacePath::try_from(name)?,
        },
    )?;
    let Response::Entry { entry } = response else {
        anyhow::bail!("stat returned {response:?}");
    };
    Ok(entry)
}

fn commit_args(handle: &[u8]) -> Vec<u8> {
    let mut args = Vec::new();
    opaque(&mut args, handle);
    args.extend_from_slice(&0_u64.to_be_bytes());
    args.extend_from_slice(&0_u32.to_be_bytes());
    args
}

/// GETATTR controls three fresh COMMITs after ordinary successful COMMIT traffic fills the replay
/// window. A live NFS export must answer new fsync/COMMIT requests instead of silently dropping them.
// contract: M1, M2, C2
#[test]
#[ignore = "ROUND100: the full replay window silently drops every fresh NFS mutation"]
fn round100_replay_window_does_not_drop_fresh_commits() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let root = rpc.handle(entry(&lab, "")?.id);
    let args = commit_args(&root);
    let timeout = Duration::from_millis(250);

    ensure!(
        rpc.getattr(&root)?.0 == 0,
        "GETATTR control failed before saturation"
    );
    let mut answered = 0;
    for _ in 0..REPLAY_LIMIT {
        let Ok(reply) = rpc.call_timeout(21, &args, timeout) else {
            break;
        };
        ensure!(status(&reply)? == 0, "COMMIT fill request failed");
        answered += 1;
    }
    ensure!(
        answered > REPLAY_LIMIT - 32,
        "replay window filled too early after {answered} replies"
    );
    ensure!(
        rpc.getattr(&root)?.0 == 0,
        "GETATTR control failed after saturation"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let result = rpc.call_timeout(21, &args, timeout);
        let replied = result.as_ref().is_ok_and(|reply| matches!(status(reply), Ok(0)));
        eprintln!("trial {trial}: fresh COMMIT replied={replied}, result={result:?}");
        failures += usize::from(!replied);
    }
    assert_eq!(failures, 0, "fresh COMMITs without an NFS reply, of 3");
    Ok(())
}

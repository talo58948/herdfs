//! Round 95: READDIRPLUS must reject a budget that cannot hold one entry.

use std::fs;

use anyhow::{Context, Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::{Rpc, opaque};

const NFS3ERR_TOOSMALL: u32 = 10_005;

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

fn readdirplus(rpc: &mut Rpc, directory: &[u8], dircount: u32, maxcount: u32) -> Result<Vec<u8>> {
    let mut args = Vec::new();
    opaque(&mut args, directory);
    args.extend_from_slice(&0_u64.to_be_bytes());
    args.extend_from_slice(&[0; 8]);
    args.extend_from_slice(&dircount.to_be_bytes());
    args.extend_from_slice(&maxcount.to_be_bytes());
    rpc.call(17, &args)
}

fn status(reply: &[u8]) -> Result<u32> {
    Ok(u32::from_be_bytes(
        reply.get(24..28).context("short READDIRPLUS reply")?.try_into()?,
    ))
}

fn empty_non_eof(reply: &[u8]) -> Result<bool> {
    let mut at = 28;
    let post_attr = u32::from_be_bytes(
        reply
            .get(at..at + 4)
            .context("short post-op attributes")?
            .try_into()?,
    );
    at += if post_attr == 0 { 4 } else { 88 };
    at += 8;
    let has_entry = u32::from_be_bytes(reply.get(at..at + 4).context("short entry list")?.try_into()?);
    at += 4;
    let eof = u32::from_be_bytes(reply.get(at..at + 4).context("short eof flag")?.try_into()?);
    Ok(has_entry == 0 && eof == 0)
}

/// A large-budget READDIRPLUS controls three single-entry directories whose long entry cannot fit
/// in the offered reply budget. The server must return TOOSMALL instead of a non-advancing page.
// contract: M1
#[test]
fn round95_readdirplus_rejects_a_budget_too_small_for_one_entry() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let long_name = "n".repeat(240);

    fs::create_dir(lab.mount_path("a").join("plus-control"))?;
    fs::write(
        lab.mount_path("a").join("plus-control").join(&long_name),
        b"control",
    )?;
    let control = rpc.handle(entry(&lab, "plus-control")?.id);
    let control_reply = readdirplus(&mut rpc, &control, 4_096, 4_096)?;
    ensure!(status(&control_reply)? == 0, "large-budget control failed");
    ensure!(
        control_reply
            .windows(long_name.len())
            .any(|bytes| bytes == long_name.as_bytes()),
        "large-budget control omitted its only entry"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let directory = format!("plus-small-{trial}");
        fs::create_dir(lab.mount_path("a").join(&directory))?;
        fs::write(lab.mount_path("a").join(&directory).join(&long_name), b"x")?;
        let handle = rpc.handle(entry(&lab, &directory)?.id);
        let reply = readdirplus(&mut rpc, &handle, 256, 256)?;
        let observed = status(&reply)?;
        ensure!(
            observed != 0 || empty_non_eof(&reply)?,
            "trial {trial} unexpectedly advanced the listing"
        );
        eprintln!("trial {trial}: small-budget READDIRPLUS returned {observed} with no entry and eof=false");
        failures += usize::from(observed != NFS3ERR_TOOSMALL);
    }
    assert_eq!(failures, 0, "non-advancing READDIRPLUS pages, of 3");
    Ok(())
}

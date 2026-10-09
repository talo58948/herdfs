//! Round 59: loopback NFS replay identity, stale parents, and durable exclusive creates.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use anyhow::{Context, Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::Rpc;

const NFS3ERR_NOENT: u32 = 2;
const NFS3ERR_STALE: u32 = 70;

fn lab() -> Result<Option<Lab>> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(None);
    }
    Ok(Some(Lab::start(
        LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream),
    )?))
}

fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn entry(lab: &Lab, name: &str) -> Result<Entry> {
    let path = if name.is_empty() {
        SpacePath::root()
    } else {
        SpacePath::try_from(name)?
    };
    let response = lab.rpc(
        "alice",
        &Request::Stat {
            space: SPACE.into(),
            path,
        },
    )?;
    let Response::Entry { entry } = response else {
        anyhow::bail!("stat returned {response:?}");
    };
    Ok(entry)
}

/// Distinct XIDs control three pairs of independent callers which use the same XID for different
/// REMOVE calls. One caller's successful reply must not suppress the other caller's mutation.
// contract: M1, C7
#[test]
fn round59_independent_loopback_callers_do_not_share_xid_replays() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    let root = entry(&lab, "")?;
    let rpc = Rpc::open(lab.client_dir("a"))?;
    let root_handle = rpc.handle(root.id);
    for name in ["xid-control-a", "xid-control-b"] {
        save(&lab.mount_path("a").join(name), b"control")?;
    }
    ensure!(rpc.remove_xid(&root_handle, "xid-control-a", 0x5900_0001)? == 0);
    ensure!(rpc.remove_xid(&root_handle, "xid-control-b", 0x5900_0002)? == 0);
    ensure!(!lab.mount_path("a").join("xid-control-b").exists());

    let mut failures = 0;
    for trial in 0..3_u32 {
        let first = format!("xid-first-{trial}");
        let second = format!("xid-second-{trial}");
        save(&lab.mount_path("a").join(&first), b"first")?;
        save(&lab.mount_path("a").join(&second), b"second")?;
        let xid = 0x5900_0100 + trial;
        ensure!(rpc.remove_xid(&root_handle, &first, xid)? == 0);
        let other = Rpc::open(lab.client_dir("a"))?;
        let status = other.remove_xid(&root_handle, &second, xid)?;
        let remains = lab.mount_path("a").join(&second).exists();
        eprintln!("trial {trial}: second REMOVE status={status}, remains={remains}");
        failures += usize::from(status == 0 && remains);
    }
    assert_eq!(failures, 0, "same-IP XID collisions that replayed another call, of 3");
    Ok(())
}

/// A missing child under a live directory controls three REMOVE calls through deleted directory
/// handles. Those handles are stale; the child is not merely absent from a live directory.
// contract: M1, C7
#[test]
fn round59_remove_through_deleted_directory_handles_is_stale() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    fs::create_dir(lab.mount_path("a").join("live-control"))?;
    let live = entry(&lab, "live-control")?;
    let rpc = Rpc::open(lab.client_dir("a"))?;
    ensure!(
        rpc.remove_xid(&rpc.handle(live.id), "missing", 0x5900_1000)? == NFS3ERR_NOENT,
        "live-directory control was not NOENT"
    );

    let mut failures = 0;
    for trial in 0..3_u32 {
        let name = format!("stale-remove-{trial}");
        fs::create_dir(lab.mount_path("a").join(&name))?;
        let directory = entry(&lab, &name)?;
        fs::remove_dir(lab.mount_path("a").join(&name))?;
        let status = rpc.remove_xid(&rpc.handle(directory.id), "missing", 0x5900_1010 + trial)?;
        eprintln!("trial {trial}: deleted-parent REMOVE returned {status}");
        failures += usize::from(status != NFS3ERR_STALE);
    }
    assert_eq!(failures, 0, "deleted-parent REMOVEs that did not return STALE, of 3");
    Ok(())
}

/// Serial EXCLUSIVE creates control three concurrent batches. Every acknowledged namespace create
/// must also durably record its verifier instead of failing on a shared identity-state temp file.
// contract: M1, C4a
#[test]
fn round59_concurrent_exclusive_creates_persist_without_colliding() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    let root = entry(&lab, "")?;
    let rpc = Rpc::open(lab.client_dir("a"))?;
    let root_handle = rpc.handle(root.id);
    for index in 0..8_u32 {
        let status = rpc.create_exclusive_xid(
            &root_handle,
            &format!("exclusive-control-{index}"),
            [index as u8; 8],
            0x5900_2000 + index,
        )?;
        ensure!(status == 0, "serial EXCLUSIVE control returned {status}");
    }

    let client = lab.client_dir("a").to_path_buf();
    let mut failed_batches = 0;
    for trial in 0..3_u32 {
        let barrier = Arc::new(Barrier::new(25));
        let mut calls = Vec::new();
        for index in 0..24_u32 {
            let client = client.clone();
            let handle = root_handle.clone();
            let barrier = barrier.clone();
            calls.push(thread::spawn(move || -> Result<u32> {
                let rpc = Rpc::open(&client)?;
                barrier.wait();
                rpc.create_exclusive_xid(
                    &handle,
                    &format!("exclusive-{trial}-{index}"),
                    [index as u8; 8],
                    0x5901_0000 + trial * 0x100 + index,
                )
            }));
        }
        barrier.wait();
        let statuses: Vec<u32> = calls
            .into_iter()
            .map(|call| call.join().map_err(|_| anyhow::anyhow!("create thread panicked"))?)
            .collect::<Result<_>>()?;
        let failed = statuses.iter().filter(|status| **status != 0).count();
        eprintln!("trial {trial}: {failed} of 24 concurrent EXCLUSIVE creates failed: {statuses:?}");
        failed_batches += usize::from(failed != 0);
    }
    assert_eq!(failed_batches, 0, "concurrent EXCLUSIVE batches with failures, of 3");
    let _: serde_json::Value = serde_json::from_slice(
        &fs::read(lab.client_dir("a").join("journal.nfs.json")).context("read NFS identity")?,
    )?;
    Ok(())
}

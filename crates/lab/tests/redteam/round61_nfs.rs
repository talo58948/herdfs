//! Round 61: the first NFS COMMIT during a refused authority outage uses the journal boundary.

use std::fs;
use std::thread;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Links, Profile, SPACE, Strategy};

use crate::VISIBLE_AFTER;
use crate::nfs_support::Rpc;

fn lab() -> Result<Option<Lab>> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(None);
    }
    Ok(Some(Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )?))
}

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

/// An online COMMIT controls three generations whose authority connection is refused after their
/// UNSTABLE WRITE. Each offline COMMIT must acknowledge the durable journal, then commit later.
// contract: C4a, C12, M2
#[test]
#[ignore = "ROUND61: first COMMIT after a refused authority connection returns NFS3ERR_IO"]
fn round61_nfs_first_offline_commit_returns_after_journaling() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    let mount = lab.mount_path("a");
    fs::write(mount.join("offline-control"), b"")?;
    lab.wait_committed("a")?;
    let control = entry(&lab, "offline-control")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let control_handle = rpc.handle(control.id);
    let write = rpc.write(&control_handle, b"online", 0)?;
    ensure!(
        write.status == 0 && write.committed == 0,
        "online control WRITE failed"
    );
    ensure!(rpc.commit(&control_handle)? == 0, "online control COMMIT failed");
    thread::sleep(VISIBLE_AFTER);
    ensure!(fs::read(lab.mount_path("b").join("offline-control"))? == b"online");

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("first-offline-commit-{trial}");
        fs::write(mount.join(&name), b"")?;
        lab.wait_committed("a")?;
        let item = entry(&lab, &name)?;
        let handle = rpc.handle(item.id);
        let bytes = format!("journalled-{trial}").into_bytes();
        let write = rpc.write(&handle, &bytes, 0)?;
        ensure!(
            write.status == 0 && write.committed == 0,
            "trial {trial} WRITE failed"
        );

        lab.cut("a", Links::Authority)?;
        let status = rpc.commit(&handle)?;
        eprintln!("trial {trial}: first offline COMMIT returned NFS status {status}");
        failures += usize::from(status != 0);
        lab.restore("a", Links::Authority)?;
        lab.wait_committed("a")?;
        thread::sleep(VISIBLE_AFTER);
        ensure!(
            fs::read(lab.mount_path("b").join(&name))? == bytes,
            "trial {trial} did not commit its journal after reconnect"
        );
    }
    assert_eq!(failures, 0, "first offline COMMIT failures, of 3");
    Ok(())
}

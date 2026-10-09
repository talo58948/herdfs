//! Round 69: READDIR through deleted directory handles.

use std::fs;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::Rpc;

const NFS3ERR_STALE: u32 = 70;

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

/// A live directory controls three READDIRs through deleted directory handles. The latter name
/// stale directory objects, not live directories that happen to contain no entries.
// contract: M1, C7
#[test]
fn round69_readdir_through_deleted_directory_handles_is_stale() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    fs::create_dir(lab.mount_path("a").join("live-control"))?;
    let live = entry(&lab, "live-control")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let live_handle = rpc.handle(live.id);
    ensure!(
        rpc.readdir_status(&live_handle, 1_024)? == 0,
        "live-directory control failed"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("stale-readdir-{trial}");
        fs::create_dir(lab.mount_path("a").join(&name))?;
        let directory = entry(&lab, &name)?;
        fs::remove_dir(lab.mount_path("a").join(&name))?;
        let handle = rpc.handle(directory.id);
        let status = rpc.readdir_status(&handle, 1_024)?;
        eprintln!("trial {trial}: deleted-parent READDIR returned {status}");
        failures += usize::from(status != NFS3ERR_STALE);
    }
    assert_eq!(
        failures, 0,
        "deleted-parent READDIRs that did not return STALE, of 3"
    );
    Ok(())
}

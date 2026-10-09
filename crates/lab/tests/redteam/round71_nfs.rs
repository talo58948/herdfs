//! Round 71: stale filehandles in NFSv3 metadata procedures.

use std::fs;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::{Rpc, opaque, status};
use crate::save;

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

fn call_status(rpc: &mut Rpc, procedure: u32, handle: &[u8], access: bool) -> Result<u32> {
    let mut args = Vec::new();
    opaque(&mut args, handle);
    if access {
        args.extend_from_slice(&0x3f_u32.to_be_bytes());
    }
    status(&rpc.call(procedure, &args)?)
}

fn statuses(rpc: &mut Rpc, handle: &[u8]) -> Result<[u32; 3]> {
    Ok([
        call_status(rpc, 4, handle, true)?,
        call_status(rpc, 18, handle, false)?,
        call_status(rpc, 20, handle, false)?,
    ])
}

/// A live handle controls three deleted handles. ACCESS, FSSTAT and PATHCONF all address the
/// object named by the handle, so deletion must make each return STALE rather than success.
// contract: M1, C7
#[test]
#[ignore = "ROUND71: ACCESS, FSSTAT and PATHCONF succeed through deleted filehandles"]
fn round71_nfs_metadata_ops_reject_deleted_handles() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    save(&lab.mount_path("a").join("live-control"), b"control")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let control = rpc.handle(entry(&lab, "live-control")?.id);
    ensure!(
        statuses(&mut rpc, &control)? == [0; 3],
        "live-handle control failed"
    );

    let mut failures = [0; 3];
    for trial in 0..3 {
        let name = format!("stale-metadata-{trial}");
        save(&lab.mount_path("a").join(&name), b"deleted")?;
        let handle = rpc.handle(entry(&lab, &name)?.id);
        fs::remove_file(lab.mount_path("a").join(&name))?;
        let observed = statuses(&mut rpc, &handle)?;
        eprintln!("trial {trial}: ACCESS/FSSTAT/PATHCONF returned {observed:?}");
        for (failures, status) in failures.iter_mut().zip(observed) {
            *failures += usize::from(status != NFS3ERR_STALE);
        }
    }
    assert_eq!(
        failures, [0; 3],
        "non-STALE replies of three trials per procedure"
    );
    Ok(())
}

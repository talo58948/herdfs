//! Round 65: duplicate-XID replay identity for identical requests from independent callers.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::Rpc;

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

/// Distinct XIDs control three pairs of independent callers that reuse one XID for byte-identical
/// REMOVEs after the name is recreated. The second caller must remove the new entry, not replay the
/// first caller's success while leaving it in place.
// contract: M1, C7
#[test]
#[ignore = "ROUND65: identical XIDs from independent callers replay across recreated entries"]
fn round65_independent_callers_do_not_replay_identical_requests() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let root = entry(&lab, "")?;
    let root_handle = Rpc::open(lab.client_dir("a"))?.handle(root.id);
    let path = lab.mount_path("a").join("replayed-remove");

    save(&path, b"first control")?;
    let first = Rpc::open(lab.client_dir("a"))?;
    ensure!(first.remove_xid(&root_handle, "replayed-remove", 0x6500_0001)? == 0);
    save(&path, b"second control")?;
    let second = Rpc::open(lab.client_dir("a"))?;
    ensure!(second.remove_xid(&root_handle, "replayed-remove", 0x6500_0002)? == 0);
    ensure!(
        !path.exists(),
        "distinct-XID control did not remove the recreated file"
    );

    let mut failures = 0;
    for trial in 0..3_u32 {
        save(&path, b"first")?;
        let xid = 0x6500_0100 + trial;
        let first = Rpc::open(lab.client_dir("a"))?;
        ensure!(first.remove_xid(&root_handle, "replayed-remove", xid)? == 0);
        save(&path, b"second")?;
        let second = Rpc::open(lab.client_dir("a"))?;
        let status = second.remove_xid(&root_handle, "replayed-remove", xid)?;
        let remains = path.exists();
        eprintln!("trial {trial}: second REMOVE status={status}, recreated file remains={remains}");
        failures += usize::from(status == 0 && remains);
        if remains {
            std::fs::remove_file(&path)?;
        }
    }
    assert_eq!(
        failures, 0,
        "independent identical REMOVEs replayed against recreated entries, of 3"
    );
    Ok(())
}

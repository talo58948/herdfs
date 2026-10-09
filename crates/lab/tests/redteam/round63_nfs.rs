//! Round 63: small, valid NFS READDIR limits fail through the protocol.

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::Rpc;

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

/// A normal READDIR controls three requests with a valid but very small byte limit. The server may
/// return TOOSMALL or an empty page, but it must send an NFS reply instead of dropping the RPC.
// contract: M1
#[test]
fn round63_nfs_small_readdir_gets_a_protocol_reply() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let root = entry(&lab, "")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let handle = rpc.handle(root.id);
    ensure!(
        rpc.readdir(&handle, 0, 300).is_ok(),
        "ordinary READDIR control failed"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let result = rpc.readdir_reply(&handle, 1, std::time::Duration::from_secs(3));
        let answered = result.is_ok();
        eprintln!("trial {trial}: one-byte READDIR answered={answered}, result={result:?}");
        failures += usize::from(!answered);
    }
    assert_eq!(failures, 0, "small READDIRs without an NFS reply, of 3");
    Ok(())
}

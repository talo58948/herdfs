//! Round 79: replaced NFS write generations must become stale.

use std::fs;
use std::thread;
use std::time::Duration;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::Rpc;
use crate::save;

const NFS3ERR_STALE: u32 = 70;
const VISIBLE: Duration = Duration::from_millis(1_200);

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

/// A live unstable write controls three old destination handles kept active while another client
/// renames a source over them. Once the replacement is visible, each old handle must be stale.
// contract: C7, M1
#[test]
fn round79_replaced_nfs_write_generation_becomes_stale() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "alice", &[], Profile::Lan, Strategy::Stream),
    )?;
    save(&lab.mount_path("b").join("write-control"), b"old")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let control = rpc.handle(entry(&lab, "write-control")?.id);
    let written = rpc.write(&control, b"live", 0)?;
    ensure!(
        written.status == 0 && written.committed == 0,
        "control WRITE failed"
    );
    let (status, bytes) = rpc.read(&control, 0, 4)?;
    ensure!(status == 0 && bytes == b"live", "live-handle control failed");

    let mut failures = 0;
    for trial in 0..3 {
        let source = format!("replace-source-{trial}");
        let destination = format!("replace-destination-{trial}");
        save(&lab.mount_path("b").join(&source), b"incoming")?;
        save(&lab.mount_path("b").join(&destination), b"old")?;
        let handle = rpc.handle(entry(&lab, &destination)?.id);
        let written = rpc.write(&handle, b"journal", 0)?;
        ensure!(
            written.status == 0 && written.committed == 0,
            "trial {trial} WRITE failed"
        );
        fs::rename(
            lab.mount_path("b").join(&source),
            lab.mount_path("b").join(&destination),
        )?;
        let mut written = None;
        for _ in 0..3 {
            thread::sleep(VISIBLE / 3);
            written = Some(rpc.write(&handle, b"journal", 0)?);
        }
        let written = written.expect("the trial sends writes");
        let (status, bytes) = rpc.read(&handle, 0, 7)?;
        eprintln!(
            "trial {trial}: replaced-handle WRITE={}, READ={status}, bytes={bytes:?}",
            written.status
        );
        failures += usize::from(written.status != NFS3ERR_STALE || status != NFS3ERR_STALE);
    }
    assert_eq!(failures, 0, "replaced write generations left usable, of 3");
    Ok(())
}

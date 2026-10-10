//! Round 77: a live NFS read epoch must not keep a deleted filehandle usable.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

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

/// A live file controls three cached read epochs kept active after another client deletes their
/// entries. Once deletion has been visible for T_vis, use of each unlinked handle must be stale.
// contract: C7, M1
#[test]
#[ignore = "ROUND77: active cached reads keep deleted NFS filehandles usable past T_vis"]
fn round77_active_read_epoch_does_not_hide_a_deleted_handle() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )?;
    save(&lab.mount_path("a").join("live-control"), b"control")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let control = rpc.handle(entry(&lab, "live-control")?.id);
    let (status, bytes) = rpc.read(&control, 0, 7)?;
    ensure!(status == 0 && bytes == b"control", "live-handle control failed");
    let control_deadline = Instant::now() + VISIBLE;
    while Instant::now() < control_deadline {
        let (status, bytes) = rpc.read(&control, 0, 7)?;
        ensure!(status == 0 && bytes == b"control", "live-handle control failed");
        thread::sleep(Duration::from_millis(200));
    }

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("deleted-active-read-{trial}");
        let bytes = format!("cached-{trial}").into_bytes();
        save(&lab.mount_path("a").join(&name), &bytes)?;
        let handle = rpc.handle(entry(&lab, &name)?.id);
        let (status, primed) = rpc.read(&handle, 0, bytes.len() as u32)?;
        ensure!(status == 0 && primed == bytes, "trial {trial} prime failed");
        fs::remove_file(lab.mount_path("b").join(&name))?;

        let deadline = Instant::now() + VISIBLE;
        let (status, observed) = loop {
            let result = rpc.read(&handle, 0, bytes.len() as u32)?;
            if result.0 == NFS3ERR_STALE || Instant::now() >= deadline {
                break result;
            }
            ensure!(result.0 == 0, "trial {trial} READ returned {}", result.0);
            thread::sleep(Duration::from_millis(200));
        };
        eprintln!("trial {trial}: read after deletion returned status={status}, bytes={observed:?}");
        failures += usize::from(status != NFS3ERR_STALE);
    }
    assert_eq!(failures, 0, "deleted filehandles still readable, of 3");
    Ok(())
}

//! Round 75: active NFS reads must keep their data-access epoch alive.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::Rpc;
use crate::save;

const IDLE: Duration = Duration::from_millis(1_200);
const READ_SIZE: usize = 384 << 10;

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

fn pending(client: &Path) -> Result<bool> {
    Ok(fs::read_dir(client.join("journal"))?
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        }))
}

fn wait_pending(client: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pending(client)? {
        ensure!(
            Instant::now() < deadline,
            "stable write did not reach the journal"
        );
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// A short completed read controls three reads which remain active beyond the idle interval. A
/// concurrent read is part of the same access and must not advance to a newly committed version.
// contract: C3, M2
#[test]
fn round75_active_nfs_read_does_not_expire_its_epoch() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let slow = Profile::Custom {
        round_trip_ms: 5,
        kilobytes_per_sec: 128,
        store_first_byte_ms: 0,
    };
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], slow, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )?;

    save(&lab.mount_path("b").join("epoch-control"), b"old")?;
    let control = entry(&lab, "epoch-control")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let control_handle = rpc.handle(control.id);
    let (status, first) = rpc.read(&control_handle, 0, 3)?;
    ensure!(status == 0 && first == b"old", "control first read failed");
    save(&lab.mount_path("b").join("epoch-control"), b"new")?;
    let (status, held) = rpc.read(&control_handle, 0, 3)?;
    ensure!(
        status == 0 && held == b"old",
        "completed-read control advanced early"
    );
    thread::sleep(IDLE);
    let (status, advanced) = rpc.read(&control_handle, 0, 3)?;
    ensure!(
        status == 0 && advanced == b"new",
        "completed-read control did not advance"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("active-read-epoch-{trial}");
        let old = vec![trial as u8; READ_SIZE];
        let new = vec![trial as u8 + 10; READ_SIZE];
        save(&lab.mount_path("b").join(&name), &old)?;
        let item = entry(&lab, &name)?;
        let handle = Rpc::open(lab.client_dir("a"))?.handle(item.id);
        let client = lab.client_dir("a").to_path_buf();
        let read_handle = handle.clone();
        let started = Instant::now();
        let reading = thread::spawn(move || -> Result<Vec<u8>> {
            let mut rpc = Rpc::open(&client)?;
            let (status, bytes) = rpc.read(&read_handle, 0, READ_SIZE as u32)?;
            ensure!(status == 0, "slow READ returned NFS status {status}");
            Ok(bytes)
        });

        thread::sleep(Duration::from_millis(400));
        save(&lab.mount_path("b").join(&name), &new)?;
        thread::sleep(IDLE.saturating_sub(started.elapsed()));
        ensure!(
            !reading.is_finished(),
            "trial {trial} slow read was no longer active"
        );
        let mut concurrent = Rpc::open(lab.client_dir("a"))?;
        let (status, second) = concurrent.read(&handle, 0, READ_SIZE as u32)?;
        ensure!(status == 0, "trial {trial} concurrent READ returned {status}");
        let first = reading
            .join()
            .map_err(|_| anyhow::anyhow!("trial {trial} slow read thread panicked"))??;
        ensure!(first == old, "trial {trial} first read changed versions");
        let changed = second == new;
        eprintln!("trial {trial}: concurrent active-epoch read advanced={changed}");
        failures += usize::from(changed);
    }
    assert_eq!(failures, 0, "active epochs advanced concurrently, of 3");
    Ok(())
}

/// A serial FILE_SYNC followed by a read controls three FILE_SYNCs with a read during their
/// commit. Settling the write must not expose the epoch's older captured head on the same client.
// contract: C1, M2
#[test]
fn round75_filesync_cannot_leave_an_old_read_epoch() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let delayed_store = Profile::Custom {
        round_trip_ms: 5,
        kilobytes_per_sec: 10_000,
        store_first_byte_ms: 500,
    };
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], delayed_store, Strategy::Stream))?;
    let bytes = vec![0x75; 256 << 10];

    fs::write(lab.mount_path("a").join("filesync-control"), b"")?;
    lab.wait_committed("a")?;
    let control = entry(&lab, "filesync-control")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let control_handle = rpc.handle(control.id);
    let written = rpc.write(&control_handle, &bytes, 2)?;
    ensure!(
        written.status == 0 && written.committed == 2,
        "serial FILE_SYNC control failed"
    );
    let (status, read) = rpc.read(&control_handle, 0, bytes.len() as u32)?;
    ensure!(
        status == 0 && read == bytes,
        "serial FILE_SYNC control was not readable"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("filesync-epoch-{trial}");
        fs::write(lab.mount_path("a").join(&name), b"")?;
        lab.wait_committed("a")?;
        let item = entry(&lab, &name)?;
        let handle = Rpc::open(lab.client_dir("a"))?.handle(item.id);
        let client = lab.client_dir("a").to_path_buf();
        let write_client = client.clone();
        let write_handle = handle.clone();
        let write_bytes = bytes.clone();
        let writing = thread::spawn(move || -> Result<_> {
            Rpc::open(&write_client)?.write(&write_handle, &write_bytes, 2)
        });
        wait_pending(&client)?;

        let mut concurrent = Rpc::open(&client)?;
        let (status, local) = concurrent.read(&handle, 0, bytes.len() as u32)?;
        ensure!(
            status == 0 && local == bytes,
            "trial {trial} did not read the local write"
        );
        let last_read = Instant::now();
        let written = writing
            .join()
            .map_err(|_| anyhow::anyhow!("trial {trial} FILE_SYNC thread panicked"))??;
        ensure!(
            written.status == 0 && written.committed == 2,
            "trial {trial} FILE_SYNC failed"
        );
        ensure!(
            last_read.elapsed() < Duration::from_millis(900),
            "trial {trial} lost the epoch window"
        );

        let (status, after) = concurrent.read(&handle, 0, bytes.len() as u32)?;
        ensure!(status == 0, "trial {trial} post-FILE_SYNC READ returned {status}");
        let stale = after.is_empty();
        eprintln!("trial {trial}: post-FILE_SYNC active epoch returned old empty head={stale}");
        failures += usize::from(stale);

        thread::sleep(IDLE);
        let (status, refreshed) = concurrent.read(&handle, 0, bytes.len() as u32)?;
        ensure!(
            status == 0 && refreshed == bytes,
            "trial {trial} idle refresh control failed"
        );
    }
    assert_eq!(failures, 0, "FILE_SYNCs exposing old epoch heads, of 3");
    Ok(())
}

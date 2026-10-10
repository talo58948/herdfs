//! Round 73: slow NFS reads must not consume their epoch's idle interval.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::Rpc;
use crate::{random_bytes, save};

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

/// A quick cached read controls three reads which each take longer than the idle interval. Time
/// spent inside a read is active time, so an immediate next read must remain on the same version.
// contract: C3, M2
#[test]
fn round73_slow_nfs_read_gets_a_full_idle_second_after_completion() -> Result<()> {
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

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("slow-read-epoch-{trial}");
        let old = random_bytes(READ_SIZE);
        let new = random_bytes(READ_SIZE);
        let newer = random_bytes(READ_SIZE);
        save(&lab.mount_path("b").join(&name), &old)?;
        let item = entry(&lab, &name)?;
        let handle = Rpc::open(lab.client_dir("a"))?.handle(item.id);
        let client = lab.client_dir("a").to_path_buf();
        let reading = thread::spawn(move || -> Result<(Duration, Vec<u8>)> {
            let mut rpc = Rpc::open(&client)?;
            let started = Instant::now();
            let (status, bytes) = rpc.read(&handle, 0, READ_SIZE as u32)?;
            ensure!(status == 0, "slow READ returned NFS status {status}");
            Ok((started.elapsed(), bytes))
        });
        thread::sleep(Duration::from_millis(500));
        save(&lab.mount_path("b").join(&name), &new)?;
        let (elapsed, first) = reading
            .join()
            .map_err(|_| anyhow::anyhow!("slow read thread panicked"))??;
        ensure!(elapsed > IDLE, "trial {trial} read was not slow: {elapsed:?}");
        ensure!(first == old, "trial {trial} first read did not keep its version");

        let mut rpc = Rpc::open(lab.client_dir("a"))?;
        let handle = rpc.handle(item.id);
        let (status, second) = rpc.read(&handle, 0, READ_SIZE as u32)?;
        ensure!(status == 0, "second READ returned NFS status {status}");
        let advanced = second == new;
        eprintln!("trial {trial}: slow read took {elapsed:?}; immediate read advanced={advanced}");
        failures += usize::from(advanced);

        thread::sleep(IDLE);
        let (status, primed) = rpc.read(&handle, 0, READ_SIZE as u32)?;
        ensure!(status == 0 && primed == new, "trial {trial} control prime failed");
        let control_started = Instant::now();
        save(&lab.mount_path("b").join(&name), &newer)?;
        ensure!(
            control_started.elapsed() < Duration::from_secs(1),
            "trial {trial} control write exceeded the idle interval"
        );
        let (status, control) = rpc.read(&handle, 0, READ_SIZE as u32)?;
        ensure!(
            status == 0 && control == new,
            "trial {trial} quick-read control advanced before one second idle"
        );
    }
    assert_eq!(failures, 0, "slow reads that consumed their idle interval, of 3");
    Ok(())
}

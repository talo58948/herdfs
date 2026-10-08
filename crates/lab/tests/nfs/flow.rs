use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Request, Response};
use herdfs_lab::{Lab, Links, Profile, SPACE};

use super::support::Rpc;
use super::{entry, lab, save};

const IDLE: Duration = Duration::from_millis(1_200);

fn history_len(lab: &Lab, name: &str) -> Result<usize> {
    let response = lab.rpc(
        "alice",
        &Request::History {
            space: SPACE.into(),
            path: SpacePath::try_from(name)?,
        },
    )?;
    let Response::History { versions } = response else {
        anyhow::bail!("history returned {response:?}");
    };
    Ok(versions.len())
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

// contract: C1, C2, M2
#[test]
fn write_stability_and_commit_are_authority_barriers() -> Result<()> {
    let slow = Profile::Custom {
        round_trip_ms: 400,
        kilobytes_per_sec: 10_000,
        store_first_byte_ms: 0,
    };
    let Some(lab) = lab(slow)? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join("barrier"), b"")?;
    let item = entry(&lab, "barrier")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let handle = rpc.handle(item.id);

    let unstable = rpc.write(&handle, b"unstable", 0)?;
    ensure!(
        unstable.status == 0 && unstable.committed == 0,
        "UNSTABLE WRITE was not passed through"
    );
    let started = Instant::now();
    ensure!(rpc.commit(&handle)? == 0, "COMMIT failed");
    ensure!(
        started.elapsed() >= Duration::from_millis(150),
        "COMMIT did not wait for the authority"
    );
    ensure!(
        entry(&lab, "barrier")?.size == 8,
        "COMMIT returned before the new head existed"
    );

    let synced = rpc.write(&handle, b"file-sync", 2)?;
    ensure!(
        synced.status == 0 && synced.committed == 2,
        "FILE_SYNC stability was not preserved"
    );
    ensure!(
        entry(&lab, "barrier")?.size == 9,
        "FILE_SYNC returned before commit"
    );
    Ok(())
}

// contract: C1, C2
#[test]
fn commit_cannot_overtake_an_earlier_write() -> Result<()> {
    let Some(lab) = lab(Profile::Lan)? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join("ordered"), b"")?;
    let item = entry(&lab, "ordered")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let handle = rpc.handle(item.id);
    let (write, commit) = rpc.write_then_commit(&handle, b"arrived first")?;
    ensure!(write.status == 0 && write.committed == 0, "WRITE failed");
    ensure!(commit == 0, "COMMIT failed");
    ensure!(entry(&lab, "ordered")?.size == 13, "COMMIT overtook WRITE");
    Ok(())
}

// contract: C1, C2, M2
#[test]
fn a_slow_write_gets_a_full_idle_second_before_sealing() -> Result<()> {
    let slow = Profile::Custom {
        round_trip_ms: 0,
        kilobytes_per_sec: 128,
        store_first_byte_ms: 0,
    };
    let Some(lab) = lab(slow)? else {
        return Ok(());
    };
    let bytes: Vec<u8> = (0..(384 << 10)).map(|index| index as u8).collect();
    save(&lab.mount_path("b").join("slow-write"), &bytes)?;
    let item = entry(&lab, "slow-write")?;
    let before = history_len(&lab, "slow-write")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let handle = rpc.handle(item.id);

    let started = Instant::now();
    ensure!(rpc.write(&handle, b"first", 0)?.status == 0, "WRITE failed");
    ensure!(
        started.elapsed() >= Duration::from_secs(1),
        "test write was not slow"
    );
    thread::sleep(Duration::from_millis(300));
    lab.wait_committed("a")?;
    let middle = history_len(&lab, "slow-write")?;
    ensure!(
        middle == before,
        "slow WRITE sealed before one second idle: {before} versions became {middle}"
    );
    ensure!(rpc.write(&handle, b"second", 0)?.status == 0, "WRITE failed");
    ensure!(rpc.commit(&handle)? == 0, "COMMIT failed");
    let after = history_len(&lab, "slow-write")?;
    ensure!(
        after == before + 1,
        "one active generation committed as multiple versions: {before} before, {after} after"
    );
    Ok(())
}

// contract: C3, M2, P1
#[test]
fn reads_advance_only_after_epoch_idle() -> Result<()> {
    let Some(lab) = lab(Profile::Lan)? else {
        return Ok(());
    };
    let a = lab.mount_path("a").join("epoch");
    let b = lab.mount_path("b").join("epoch");
    save(&a, b"first")?;
    thread::sleep(IDLE);
    assert_eq!(fs::read(&b)?, b"first");
    save(&a, b"second")?;
    assert_eq!(fs::read(&b)?, b"first", "active epoch changed versions");
    thread::sleep(IDLE);
    assert_eq!(fs::read(&b)?, b"second");
    Ok(())
}

// contract: M2, C12
#[test]
fn offline_commit_returns_once_the_generation_is_journaled() -> Result<()> {
    let Some(lab) = lab(Profile::Lan)? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join("offline"), b"online")?;
    let item = entry(&lab, "offline")?;
    let root = entry(&lab, "")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let handle = rpc.handle(item.id);
    ensure!(rpc.write(&handle, b"offline", 0)?.status == 0, "WRITE failed");
    lab.silence("a", Links::Authority)?;
    let _ = rpc.getattr(&rpc.handle(root.id));

    let started = Instant::now();
    let status = rpc.commit(&handle)?;
    ensure!(status == 0, "offline COMMIT returned NFS status {status}");
    ensure!(
        started.elapsed() < Duration::from_secs(2),
        "offline COMMIT blocked"
    );
    ensure!(
        pending(lab.client_dir("a"))?,
        "offline COMMIT left no journal record"
    );
    lab.restore("a", Links::Authority)?;
    lab.wait_committed("a")?;
    thread::sleep(IDLE);
    assert_eq!(fs::read(lab.mount_path("b").join("offline"))?, b"offline");
    Ok(())
}

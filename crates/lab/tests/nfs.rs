//! Focused loopback NFSv3 behavior on the Linux system client.

#[path = "nfs/support.rs"]
mod support;

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Links, Profile, SPACE, Strategy};

use support::{Rpc, denied_auth};

const IDLE: Duration = Duration::from_millis(1_200);
const NFS3ERR_STALE: u32 = 70;

fn lab(profile: Profile) -> Result<Option<Lab>> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(None);
    }
    Ok(Some(Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], profile, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )?))
}

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

// contract: M2, P1, C2
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

// contract: M1
#[test]
fn readdir_cookies_continue_and_restart_after_deletion() -> Result<()> {
    let Some(lab) = lab(Profile::Lan)? else {
        return Ok(());
    };
    for name in ["cookie-a", "cookie-b", "cookie-c", "cookie-d"] {
        save(&lab.mount_path("a").join(name), name.as_bytes())?;
    }
    let root = entry(&lab, "")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let handle = rpc.handle(root.id);
    let first = rpc.readdir(&handle, 0, 300)?;
    let (cookie, removed) = first.last().cloned().context("empty first READDIR page")?;
    let continued = rpc.readdir(&handle, cookie, 1_024)?;
    ensure!(
        continued.iter().all(|(next, _)| *next != cookie),
        "READDIR repeated its cookie"
    );

    fs::remove_file(lab.mount_path("a").join(&removed))?;
    let restarted = rpc.readdir(&handle, cookie, 300)?;
    let fresh = rpc.readdir(&handle, 0, 300)?;
    ensure!(restarted == fresh, "a missing cookie did not restart the listing");
    Ok(())
}

// contract: M1
#[test]
fn remove_and_rmdir_report_the_target_kind() -> Result<()> {
    let Some(lab) = lab(Profile::Lan)? else {
        return Ok(());
    };
    let mount = lab.mount_path("a");
    fs::create_dir(mount.join("directory"))?;
    save(&mount.join("file"), b"x")?;
    let remove_dir = fs::remove_file(mount.join("directory")).unwrap_err();
    let rmdir_file = fs::remove_dir(mount.join("file")).unwrap_err();
    ensure!(
        remove_dir.raw_os_error() == Some(libc::EISDIR),
        "REMOVE dir was not EISDIR"
    );
    ensure!(
        rmdir_file.raw_os_error() == Some(libc::ENOTDIR),
        "RMDIR file was not ENOTDIR"
    );
    Ok(())
}

// contract: M1
#[test]
fn exclusive_create_replays_after_restart_and_auth_null_is_refused() -> Result<()> {
    let Some(mut lab) = lab(Profile::Lan)? else {
        return Ok(());
    };
    let root = entry(&lab, "")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    ensure!(denied_auth(&rpc.auth_null()?)?, "AUTH_NULL was accepted");
    let root_handle = rpc.handle(root.id);
    let (status, first) = rpc.create_exclusive(&root_handle, "exclusive", [7; 8])?;
    ensure!(status == 0, "EXCLUSIVE CREATE failed");
    let first = first.context("EXCLUSIVE CREATE returned no handle")?;

    lab.restart_client("a")?;
    let mut restarted = Rpc::open(lab.client_dir("a"))?;
    let (status, replay) = restarted.create_exclusive(&root_handle, "exclusive", [7; 8])?;
    ensure!(
        status == 0 && replay.as_deref() == Some(first.as_slice()),
        "EXCLUSIVE replay changed"
    );
    Ok(())
}

// contract: M1
#[test]
fn handles_survive_restart_without_lookup_and_deleted_handles_are_stale() -> Result<()> {
    let Some(mut lab) = lab(Profile::Lan)? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join("held"), b"before restart")?;
    let item = entry(&lab, "held")?;
    let rpc = Rpc::open(lab.client_dir("a"))?;
    let handle = rpc.handle(item.id);
    lab.restart_client("a")?;

    let mut restarted = Rpc::open(lab.client_dir("a"))?;
    let (status, size) = restarted.getattr(&handle)?;
    ensure!(status == 0 && size == Some(14), "old handle GETATTR failed");
    let (status, bytes) = restarted.read(&handle, 0, 128)?;
    ensure!(
        status == 0 && bytes == b"before restart",
        "old handle READ failed"
    );
    let written = restarted.write(&handle, b"after restart", 2)?;
    ensure!(
        written.status == 0 && written.committed == 2,
        "old handle WRITE failed"
    );

    fs::remove_file(lab.mount_path("a").join("held"))?;
    let (status, _) = restarted.getattr(&handle)?;
    ensure!(
        status == NFS3ERR_STALE,
        "deleted handle returned NFS status {status}"
    );
    Ok(())
}

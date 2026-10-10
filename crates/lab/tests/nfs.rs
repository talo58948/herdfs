//! Focused loopback NFSv3 behavior on the Linux system client.

#[path = "nfs/flow.rs"]
mod flow;
#[path = "nfs/support.rs"]
mod support;

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use support::{Rpc, denied_auth};

const NFS3ERR_STALE: u32 = 70;
const NFS3ERR_NOENT: u32 = 2;

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
        remove_dir.raw_os_error()
            == Some(if cfg!(target_os = "macos") {
                libc::EPERM
            } else {
                libc::EISDIR
            }),
        "REMOVE dir returned {remove_dir}"
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

/// A missing child under a live directory controls three lookups through deleted directory
/// handles. The latter must identify the handle as stale, not describe the child as absent.
// contract: M1
#[test]
fn round57_deleted_directory_handles_return_stale_from_lookup() -> Result<()> {
    let Some(lab) = lab(Profile::Lan)? else {
        return Ok(());
    };
    let root = entry(&lab, "")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let root_handle = rpc.handle(root.id);
    ensure!(
        rpc.lookup(&root_handle, "missing-control")? == NFS3ERR_NOENT,
        "live-directory control was not NOENT"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("stale-lookup-{trial}");
        fs::create_dir(lab.mount_path("a").join(&name))?;
        let directory = entry(&lab, &name)?;
        let handle = rpc.handle(directory.id);
        fs::remove_dir(lab.mount_path("a").join(&name))?;
        let observed = rpc.lookup(&handle, "child")?;
        eprintln!("trial {trial}: deleted-directory LOOKUP returned {observed}");
        failures += usize::from(observed != NFS3ERR_STALE);
    }
    assert_eq!(
        failures, 0,
        "deleted-directory LOOKUPs that did not return STALE, of 3"
    );
    Ok(())
}

/// Reusing an XID on one connection controls three reconnect retransmissions of REMOVE. A
/// retransmission must replay success instead of executing the mutation again and seeing NOENT.
// contract: M1
#[test]
fn round57_duplicate_xid_replays_after_a_tcp_reconnect() -> Result<()> {
    let Some(lab) = lab(Profile::Lan)? else {
        return Ok(());
    };
    let root = entry(&lab, "")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let root_handle = rpc.handle(root.id);
    save(&lab.mount_path("a").join("xid-control"), b"control")?;
    ensure!(
        rpc.remove_replayed(&root_handle, "xid-control", false)? == [0, 0],
        "same-connection duplicate control was not replayed"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("xid-reconnect-{trial}");
        save(&lab.mount_path("a").join(&name), b"attack")?;
        let observed = rpc.remove_replayed(&root_handle, &name, true)?;
        eprintln!("trial {trial}: reconnect duplicate REMOVE returned {observed:?}");
        failures += usize::from(observed != [0, 0]);
    }
    assert_eq!(
        failures, 0,
        "reconnect duplicate REMOVEs that were executed twice, of 3"
    );
    Ok(())
}

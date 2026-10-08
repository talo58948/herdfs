//! Round 39: held-entry, Pin, and journal crash probes through FUSE.

use std::fs::{self, File, OpenOptions};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy};

use crate::{
    VISIBLE_AFTER, assert_bytes, random_bytes, read_at, read_held, save, save_entry, two_clients,
    write_entry,
};

const TRIALS: usize = 3;

fn wait_pending(lab: &herdfs_lab::Lab) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while lab.stats("b")?["pending_commits"] != 1 {
        ensure!(Instant::now() < deadline, "truncate was not journaled");
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn list_name(path: &std::path::Path, name: &str) -> Result<()> {
    let listed = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(listed.iter().any(|listed| listed == name), "listed {name}");
    Ok(())
}

fn open_with_create(path: &std::path::Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// A listing may cache an entry without giving the kernel a lookup reference. If another client
/// removes that entry and recreates its name, O_CREAT must open the entry that exists now. A
/// listed but unchanged file is the control for three remove-and-recreate trials.
// contract: C2, C7
#[test]
fn create_existing_opens_a_recreated_listed_name() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));

    save(&a.join("control"), b"control")?;
    list_name(b, "control")?;
    let control = open_with_create(&b.join("control"))?;
    assert_bytes(
        &read_held(&control)?,
        b"control",
        "control",
    );

    let mut failures = 0;
    for trial in 0..TRIALS {
        let name = format!("recreated-{trial}");
        let replacement = format!("replacement-{trial}");
        save(&a.join(&name), b"old")?;
        list_name(b, &name)?;
        fs::remove_file(a.join(&name))?;
        save(&a.join(&name), replacement.as_bytes())?;

        let opened = open_with_create(&b.join(&name));
        let read = opened
            .as_ref()
            .ok()
            .and_then(|file| read_held(file).ok());
        eprintln!("trial {trial}: open={opened:?}, read={read:?}");
        failures += usize::from(read.as_deref() != Some(replacement.as_bytes()));
    }
    assert_eq!(
        failures, 0,
        "O_CREAT calls that missed the recreated entry, of {TRIALS}"
    );
    Ok(())
}

/// Repro: b opens an existing file with O_TRUNC, writes nothing, and closes it while offline.
/// The control waits for FUSE release, reconnects, and commits the empty file. Each attack
/// SIGKILLs b as soon as close returns; recovery finds no journal record and the authority keeps
/// "old". Dirty writes seal in flush, but a truncate without a write is deferred until release
/// (`fs/handles.rs`), which the kernel may deliver after close has returned.
// contract: C4a, C12
#[test]
fn truncate_only_close_survives_an_immediate_client_crash() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a").to_owned(), lab.mount_path("b").to_owned());

    save(&a.join("truncate-control"), b"old")?;
    thread::sleep(VISIBLE_AFTER);
    fs::metadata(b.join("truncate-control"))?;
    lab.cut("b", Links::Authority)?;
    drop(File::create(b.join("truncate-control"))?);
    wait_pending(&lab)?;
    lab.restore("b", Links::Authority)?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(&fs::read(a.join("truncate-control"))?, b"", "control");

    let mut failures = 0;
    for trial in 0..TRIALS {
        let name = format!("truncate-crash-{trial}");
        save(&a.join(&name), b"old")?;
        thread::sleep(VISIBLE_AFTER);
        fs::metadata(b.join(&name))?;
        lab.cut("b", Links::Authority)?;
        drop(File::create(b.join(&name))?);
        lab.kill_client("b")?;
        lab.restore("b", Links::Authority)?;
        lab.restart_client("b")?;
        lab.wait_committed("b")?;
        thread::sleep(VISIBLE_AFTER);
        let bytes = fs::read(a.join(&name))?;
        eprintln!("trial {trial}: {} bytes after recovery", bytes.len());
        failures += usize::from(!bytes.is_empty());
    }
    assert_eq!(
        failures, 0,
        "truncate-only closes lost across a crash, of {TRIALS}"
    );
    Ok(())
}

fn offline_child_after_create(
    lab: &herdfs_lab::Lab,
    trial: usize,
    moved: bool,
) -> Result<(std::io::Result<()>, Vec<u8>)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let old = format!("created-parent-{moved}-{trial}");
    let new = format!("created-parent-moved-{moved}-{trial}");
    fs::create_dir(a.join(&old))?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join(&old))?;
    let authoritative = if moved {
        fs::rename(a.join(&old), a.join(&new))?;
        fs::create_dir(a.join(&old))?;
        a.join(&new).join("child")
    } else {
        a.join(&old).join("child")
    };
    save_entry(&held, "child", b"online")?;
    thread::sleep(VISIBLE_AFTER);

    lab.cut("b", Links::Authority)?;
    let written = write_entry(&held, "child", b"offline");
    lab.restore("b", Links::Authority)?;
    if written.is_ok() {
        lab.wait_committed("b")?;
        thread::sleep(VISIBLE_AFTER);
    }
    Ok((written, fs::read(authoritative)?))
}

/// Repro: a moves and recreates a directory b holds, then b creates and syncs `child` through its
/// descriptor. The authority's answer puts the child at the moved path, but does not rebind the
/// parent. After TTL, offline lookup constructs the old `parent/child` path and returns EIO. The
/// stationary-parent control rewrites and commits the child offline.
// contract: C7, C12
#[test]
fn a_child_created_below_a_moved_parent_can_be_reopened_offline() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (written, bytes) = offline_child_after_create(&lab, 0, false)?;
    ensure!(written.is_ok(), "control offline rewrite: {written:?}");
    assert_bytes(&bytes, b"offline", "control bytes");

    let mut failures = 0;
    for trial in 0..TRIALS {
        let (written, bytes) = offline_child_after_create(&lab, trial, true)?;
        eprintln!("trial {trial}: write={written:?}, bytes={bytes:?}");
        failures += usize::from(written.is_err() || bytes != b"offline");
    }
    assert_eq!(
        failures, 0,
        "offline reopens that lost the created child, of {TRIALS}"
    );
    Ok(())
}

fn pin_lab() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Pin)
        .cache_bytes("b", 1 << 20);
    Lab::start(config).map(Some)
}

fn pinned_after_parent_move(lab: &Lab, trial: usize, moved: bool) -> Result<bool> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let old = format!("pin-parent-{moved}-{trial}");
    let new = format!("pin-parent-moved-{moved}-{trial}");
    fs::create_dir(a.join(&old))?;
    save(&a.join(&old).join("file"), &random_bytes(512 << 10))?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join(&old).join("file"))?;

    let authoritative = if moved {
        fs::rename(a.join(&old), a.join(&new))?;
        fs::create_dir(a.join(&old))?;
        save(&a.join(&old).join("file"), b"replacement")?;
        a.join(&new).join("file")
    } else {
        a.join(&old).join("file")
    };
    let bytes = random_bytes(8 << 20);
    save(&authoritative, &bytes)?;
    thread::sleep(VISIBLE_AFTER);
    ensure!(held.metadata()?.len() == bytes.len() as u64, "new size");
    assert_bytes(&read_at(&held, 0, bytes.len())?, &bytes, "new version");
    lab.cut("b", Links::Store)?;
    let reread = read_at(&held, 0, 4096);
    lab.restore("b", Links::Store)?;
    Ok(reread.is_ok_and(|page| page == bytes[..4096]))
}

/// A Pin handle keeps its strategy while its entry follows both a parent rename and a new
/// version. A stationary parent controls three rename-and-recreate trials under cache pressure.
// contract: C3, C7, P1
#[test]
fn a_pin_handle_stays_pinned_across_a_parent_move_and_version_switch() -> Result<()> {
    let Some(lab) = pin_lab()? else {
        return Ok(());
    };
    ensure!(pinned_after_parent_move(&lab, 0, false)?, "control stays pinned");
    let mut failures = 0;
    for trial in 0..TRIALS {
        failures += usize::from(!pinned_after_parent_move(&lab, trial, true)?);
    }
    assert_eq!(failures, 0, "moved Pin handles that lost chunks, of {TRIALS}");
    Ok(())
}

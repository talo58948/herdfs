//! Round 67: colliding kept roots across a replacing directory rename.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy, herdfs_command};
use serde_json::Value;

use crate::{VISIBLE_AFTER, random_bytes, save};

const WAIT: Duration = Duration::from_secs(30);

fn command(op: &str, path: &Path) -> Result<Value> {
    let output = herdfs_command()?.args(["--json", op]).arg(path).output()?;
    ensure!(
        output.status.success(),
        "{op} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn status(lab: &Lab) -> Result<Value> {
    command("status", lab.mount_path("b"))
}

fn wait_ready(lab: &Lab, roots: &[&str]) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = status(lab)?;
        let ready = roots.iter().all(|root| {
            state["kept"]["pins"]
                .as_array()
                .and_then(|pins| pins.iter().find(|pin| pin["path"] == *root))
                .is_some_and(|pin| pin["ready"] == true)
        });
        if ready {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "pins did not become ready: {state}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_settled(lab: &Lab, old: &str) -> Result<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = status(lab)?;
        let old_present = state["kept"]["pins"]
            .as_array()
            .is_some_and(|pins| pins.iter().any(|pin| pin["path"] == old));
        if !old_present {
            return Ok(state);
        }
        ensure!(Instant::now() < deadline, "renamed pin did not settle: {state}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn pin(lab: &Lab, path: &str) -> Result<()> {
    ensure!(command("pin", &lab.mount_path("b").join(path))?["ok"] == true);
    Ok(())
}

/// A rename onto an unused name controls three renames onto separately pinned empty directories.
/// Replacing the destination must not discard the pin that follows the source entry.
// contract: P4, C7
#[test]
#[ignore = "ROUND67: replacing a separately pinned destination loses the source pin"]
fn round67_replacing_rename_keeps_the_source_pin() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() == Ok("nfs") {
        eprintln!("skipped: the default-mount probe runs separately from NFS");
        return Ok(());
    }
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
            .cache_bytes("b", 128 << 10),
    )?;

    let control_old = "a-control-old";
    let control_new = "z-control-new";
    fs::create_dir(lab.mount_path("a").join(control_old))?;
    save(
        &lab.mount_path("a").join(control_old).join("file"),
        &random_bytes(512 << 10),
    )?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    pin(&lab, control_old)?;
    wait_ready(&lab, &[control_old])?;
    fs::rename(
        lab.mount_path("a").join(control_old),
        lab.mount_path("a").join(control_new),
    )?;
    let control = wait_settled(&lab, control_old)?;
    ensure!(
        control["kept"]["pins"]
            .as_array()
            .is_some_and(|pins| pins.iter().any(|pin| pin["path"] == control_new)),
        "ordinary rename control lost its source pin: {control}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let source = format!("a-source-{trial}");
        let destination = format!("z-destination-{trial}");
        fs::create_dir(lab.mount_path("a").join(&source))?;
        fs::create_dir(lab.mount_path("a").join(&destination))?;
        save(
            &lab.mount_path("a").join(&source).join("file"),
            &random_bytes(512 << 10),
        )?;
        lab.wait_committed("a")?;
        thread::sleep(VISIBLE_AFTER);
        pin(&lab, &source)?;
        pin(&lab, &destination)?;
        wait_ready(&lab, &[&source, &destination])?;

        fs::rename(
            lab.mount_path("a").join(&source),
            lab.mount_path("a").join(&destination),
        )?;
        let state = wait_settled(&lab, &source)?;
        let kept = state["kept"]["pins"]
            .as_array()
            .is_some_and(|pins| pins.iter().any(|pin| pin["path"] == destination));
        eprintln!("trial {trial}: destination kept={kept}, status={state}");
        failures += usize::from(!kept);
    }
    assert_eq!(failures, 0, "replacing renames that lost the source pin, of 3");
    Ok(())
}

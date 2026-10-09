//! Round 57: durable keep-downloaded roots across namespace renames.

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

fn wait_ready(lab: &Lab, root: &str) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = status(lab)?;
        if state["kept"]["pins"]
            .as_array()
            .and_then(|pins| pins.iter().find(|pin| pin["path"] == root))
            .is_some_and(|pin| pin["ready"] == true)
        {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "{root} did not become ready: {state}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_error(path: &Path) -> Result<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = command("file", path)?;
        if !state["error"].is_null() {
            return Ok(state);
        }
        ensure!(Instant::now() < deadline, "renamed pin did not settle: {state}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn pin_folder(lab: &Lab, name: &str) -> Result<()> {
    let folder = lab.mount_path("a").join(name);
    fs::create_dir(&folder)?;
    save(&folder.join("file"), &random_bytes(256 << 10))?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    command("pin", &lab.mount_path("b").join(name))?;
    wait_ready(lab, name)
}

/// A stationary kept folder controls three roots renamed by another client. A rename must not
/// turn a kept folder into an errored old-path pin while the same entry at its new path is unkept.
// contract: P4, C7
#[test]
fn round57_kept_folder_follows_its_rename() -> Result<()> {
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

    pin_folder(&lab, "kept-control")?;
    let control = command("file", &lab.mount_path("b").join("kept-control"))?;
    ensure!(
        control["kept"] == true && control["error"].is_null(),
        "control pin failed: {control}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let old = format!("kept-old-{trial}");
        let new = format!("kept-new-{trial}");
        pin_folder(&lab, &old)?;
        fs::rename(lab.mount_path("a").join(&old), lab.mount_path("a").join(&new))?;
        command("pin", &lab.mount_path("b").join(&old))?;
        let old_state = wait_error(&lab.mount_path("b").join(&old))?;
        let new_state = command("file", &lab.mount_path("b").join(&new))?;
        eprintln!("trial {trial}: old={old_state}, new={new_state}");
        failures += usize::from(new_state["kept"] != true || !new_state["error"].is_null());
    }
    assert_eq!(failures, 0, "renamed kept roots that lost their pin, of 3");
    Ok(())
}

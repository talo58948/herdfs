//! Round 65: ready keep-downloaded data after cache integrity failures.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_lab::{ADMIN, Lab, LabConfig, Links, Profile, Strategy, herdfs_command};
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

fn wait_ready(lab: &Lab) -> Result<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = command("status", lab.mount_path("b"))?;
        if state["kept"]["pins"]
            .as_array()
            .is_some_and(|pins| pins.len() == 1 && pins[0]["ready"] == true)
        {
            return Ok(state);
        }
        ensure!(
            Instant::now() < deadline,
            "kept root did not become ready: {state}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// An intact ready file controls three ready files whose cached first chunk is corrupted. Status
/// must not still advertise them as offline-readable after cache verification can no longer pass.
// contract: P4, C11
#[test]
#[ignore = "ROUND65: status stays ready after kept chunks fail cache verification"]
fn round65_kept_ready_requires_verified_chunks() -> Result<()> {
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
    let root_a = lab.mount_path("a").join("kept-integrity");
    let root_b = lab.mount_path("b").join("kept-integrity");
    fs::create_dir(&root_a)?;
    let control = random_bytes(2 << 20);
    save(&root_a.join("control"), &control)?;
    let attacks: Vec<_> = (0..3)
        .map(|trial| (format!("corrupt-{trial}"), random_bytes(2 << 20)))
        .collect();
    for (name, bytes) in &attacks {
        save(&root_a.join(name), bytes)?;
    }
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    command("pin", &root_b)?;
    wait_ready(&lab)?;

    lab.cut("b", Links::Both)?;
    ensure!(
        fs::read(root_b.join("control"))? == control,
        "intact ready control was not offline"
    );
    lab.restore("b", Links::Both)?;

    let cache = lab.client_dir("b").join("cache");
    for (name, _) in &attacks {
        let version = lab.head(ADMIN, &format!("kept-integrity/{name}"))?;
        let chunk = version.chunks.first().expect("non-empty attack file");
        fs::write(cache.join(chunk.id.to_string()), vec![0; chunk.len as usize])?;
    }
    let before = wait_ready(&lab)?;
    lab.cut("b", Links::Both)?;
    let mut failures = 0;
    for (trial, (name, bytes)) in attacks.iter().enumerate() {
        let read = fs::read(root_b.join(name));
        let offline = read.as_ref().is_ok_and(|got| got == bytes);
        eprintln!(
            "trial {trial}: ready={}, corrupted file offline={offline}, result={:?}",
            before["kept"]["pins"][0]["ready"],
            read.as_ref().map(Vec::len)
        );
        failures += usize::from(!offline);
    }
    assert_eq!(
        failures, 0,
        "ready corrupted files that were not readable offline, of 3"
    );
    Ok(())
}

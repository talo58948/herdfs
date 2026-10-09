//! Round 69: kept-downloaded accounting while a newer version downloads.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy, herdfs_command};
use serde_json::Value;

use crate::{VISIBLE_AFTER, random_bytes, save};

const WAIT: Duration = Duration::from_secs(60);

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

fn wait_ready(lab: &Lab) -> Result<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = status(lab)?;
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
        thread::sleep(Duration::from_millis(20));
    }
}

fn observe_refresh(lab: &Lab) -> Result<(bool, Value)> {
    let deadline = Instant::now() + WAIT;
    let mut pending = false;
    let mut mismatch = None;
    loop {
        let state = status(lab)?;
        let ready = state["kept"]["pins"][0]["ready"] == true;
        pending |= !ready;
        let used = state["cache"]["used"].as_u64();
        let kept = state["kept"]["bytes"].as_u64();
        if pending && used != kept {
            mismatch = Some(state.clone());
        }
        if pending && ready {
            return Ok((mismatch.is_some(), mismatch.unwrap_or(state)));
        }
        ensure!(
            Instant::now() < deadline,
            "new kept version did not settle: {state}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// A settled pin controls three newer versions. During each download, cache use and kept use must
/// still agree because no unkept data was accessed and both retained versions are pinned.
// contract: P4, U1
#[test]
#[ignore = "ROUND69: kept use omits the retired version during replacement download"]
fn round69_kept_use_counts_the_retired_download_version() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() == Ok("nfs") {
        eprintln!("skipped: the default-mount probe runs separately from NFS");
        return Ok(());
    }
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let slow_store = Profile::Custom {
        round_trip_ms: 5,
        kilobytes_per_sec: 512,
        store_first_byte_ms: 10,
    };
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], slow_store, Strategy::Stream)
            .cache_bytes("b", 64 << 10),
    )?;
    let path_a = lab.mount_path("a").join("kept-accounting");
    let path_b = lab.mount_path("b").join("kept-accounting");
    save(&path_a, &random_bytes(2 << 20))?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    command("pin", &path_b)?;
    let control = wait_ready(&lab)?;
    ensure!(
        control["cache"]["used"] == control["kept"]["bytes"],
        "settled control accounting differed: {control}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        save(&path_a, &random_bytes(4 << 20))?;
        lab.wait_committed("a")?;
        thread::sleep(VISIBLE_AFTER);
        let (differed, state) = observe_refresh(&lab)?;
        eprintln!(
            "trial {trial}: cache used={}, kept bytes={}, differed={differed}",
            state["cache"]["used"], state["kept"]["bytes"]
        );
        failures += usize::from(differed);
    }
    assert_eq!(failures, 0, "refreshes that under-reported kept use, of 3");
    Ok(())
}

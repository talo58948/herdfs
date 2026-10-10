//! Round 79: one kept-root download must not hide changes below another root.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy, herdfs_command};
use serde_json::Value;

use crate::{VISIBLE_AFTER, random_bytes, save};

const WAIT: Duration = Duration::from_secs(45);

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

fn ready(state: &Value, root: &str) -> Result<bool> {
    state["kept"]["pins"]
        .as_array()
        .context("status has no kept pins")?
        .iter()
        .find(|pin| pin["path"] == root)
        .and_then(|pin| pin["ready"].as_bool())
        .with_context(|| format!("status has no {root} pin: {state}"))
}

fn wait_until(lab: &Lab, root: &str, expected: bool) -> Result<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = status(lab)?;
        if ready(&state, root)? == expected {
            return Ok(state);
        }
        ensure!(
            Instant::now() < deadline,
            "{root} ready never became {expected}: {state}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// Existing offline bytes control three children committed below a second ready root while the
/// first root downloads slowly. Each root's readiness must reflect its own latest membership.
// contract: P4, C2
#[test]
#[ignore = "ROUND79: one slow kept root blocks readiness checks for another root"]
fn round79_slow_kept_root_does_not_hide_another_roots_new_child() -> Result<()> {
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
        kilobytes_per_sec: 256,
        store_first_byte_ms: 10,
    };
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "alice", &[], slow_store, Strategy::Stream)
            .cache_bytes("b", 64 << 10),
    )?;
    let slow_a = lab.mount_path("a").join("a-slow");
    let late_a = lab.mount_path("a").join("z-late");
    let late_b = lab.mount_path("b").join("z-late");
    fs::create_dir(&slow_a)?;
    fs::create_dir(&late_a)?;
    save(&slow_a.join("large"), b"small")?;
    save(&late_a.join("control"), b"control")?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    command("pin", &lab.mount_path("b").join("a-slow"))?;
    command("pin", &late_b)?;
    wait_until(&lab, "a-slow", true)?;
    wait_until(&lab, "z-late", true)?;

    lab.cut("b", Links::Both)?;
    ensure!(
        fs::read(late_b.join("control"))? == b"control",
        "ready control was not offline"
    );
    lab.restore("b", Links::Both)?;

    let mut failures = 0;
    for trial in 0..3 {
        save(&slow_a.join("large"), &random_bytes(2 << 20))?;
        wait_until(&lab, "a-slow", false)?;
        let name = format!("late-{trial}");
        let expected = format!("child-{trial}");
        save(&late_a.join(&name), expected.as_bytes())?;
        thread::sleep(VISIBLE_AFTER);
        let state = status(&lab)?;
        let advertised = ready(&state, "z-late")?;

        lab.cut("b", Links::Both)?;
        let read = fs::read(late_b.join(&name));
        let offline = read.as_ref().is_ok_and(|bytes| bytes == expected.as_bytes());
        lab.restore("b", Links::Both)?;
        eprintln!("trial {trial}: second root ready={advertised}, new child offline={offline}");
        failures += usize::from(advertised && !offline);

        wait_until(&lab, "a-slow", true)?;
        wait_until(&lab, "z-late", true)?;
    }
    assert_eq!(failures, 0, "ready roots missing new offline children, of 3");
    Ok(())
}

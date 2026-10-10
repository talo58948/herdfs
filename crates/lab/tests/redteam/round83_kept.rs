//! Round 83: a ready kept folder must remain listable offline.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy, herdfs_command};
use serde_json::Value;

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

fn wait_ready(lab: &Lab) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = command("status", lab.mount_path("reader"))?;
        let pins = state["kept"]["pins"]
            .as_array()
            .context("status has no kept pins")?;
        if pins.len() == 4 && pins.iter().all(|pin| pin["ready"] == true) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "kept folders did not become ready: {state}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn names(path: &Path) -> Result<Vec<String>> {
    let mut names = fs::read_dir(path)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

/// An online listing controls three separately kept folders. Ready must include enough namespace
/// metadata to list each folder, not only enough metadata to open a child by a known name.
// contract: P4, M1
#[test]
#[ignore = "ROUND83: ready kept folders cannot be listed offline"]
fn round83_ready_kept_folders_are_listable_offline() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("writer", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("reader", "alice", &[], Profile::Lan, Strategy::Stream),
    )?;
    for root in ["control", "trial-0", "trial-1", "trial-2"] {
        let path = lab.mount_path("writer").join(root);
        fs::create_dir(&path)?;
        fs::write(path.join("child"), root)?;
    }
    lab.wait_committed("writer")?;
    for root in ["control", "trial-0", "trial-1", "trial-2"] {
        command("pin", &lab.mount_path("reader").join(root))?;
    }
    wait_ready(&lab)?;
    ensure!(
        names(&lab.mount_path("reader").join("control"))? == ["child"],
        "online control listing was incomplete"
    );

    lab.cut("reader", Links::Both)?;
    thread::sleep(Duration::from_millis(1_200));
    let mut failures = 0;
    for trial in 0..3 {
        let listed = names(&lab.mount_path("reader").join(format!("trial-{trial}")));
        let complete = listed.as_ref().is_ok_and(|names| names == &["child"]);
        eprintln!("trial {trial}: offline listing complete={complete}, result={listed:?}");
        failures += usize::from(!complete);
    }
    assert_eq!(failures, 0, "ready kept folders not listable offline, of 3");
    Ok(())
}

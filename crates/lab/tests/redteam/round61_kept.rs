//! Round 61: ready keep-downloaded roots must not lag a newer committed head.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy, herdfs_command};
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

fn wait_ready(lab: &Lab) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = status(lab)?;
        if state["kept"]["pins"]
            .as_array()
            .is_some_and(|pins| pins.len() == 1 && pins[0]["ready"] == true)
        {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "kept root did not become ready: {state}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// A refresh followed by an offline read controls three newer heads observed after T_vis. Status
/// must not keep reporting ready while only the preceding version is available offline.
// contract: P4, C2, C12
#[test]
#[ignore = "ROUND61: status stays ready while a newer committed head is not cached"]
fn round61_kept_ready_tracks_the_latest_committed_head() -> Result<()> {
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
    let folder = lab.mount_path("a").join("kept-latest");
    fs::create_dir(&folder)?;
    let path_a = folder.join("file");
    let root_b = lab.mount_path("b").join("kept-latest");
    let path_b = root_b.join("file");
    let expected = random_bytes(512 << 10);
    save(&path_a, &expected)?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    command("pin", &root_b)?;
    wait_ready(&lab)?;

    lab.cut("b", Links::Both)?;
    ensure!(
        fs::read(&path_b)? == expected,
        "ready control was not readable offline"
    );
    lab.restore("b", Links::Both)?;

    let mut stale = 0;
    for trial in 0..3 {
        let newer = random_bytes(512 << 10);
        save(&path_a, &newer)?;
        lab.wait_committed("a")?;
        thread::sleep(VISIBLE_AFTER);
        let before = status(&lab)?;
        ensure!(
            before["kept"]["pins"][0]["ready"] == true,
            "trial {trial} was not advertised ready before the offline read: {before}"
        );
        lab.cut("b", Links::Both)?;
        let got = fs::read(&path_b)?;
        eprintln!(
            "trial {trial}: ready after the newer commit={}, offline bytes are latest={}",
            before["kept"]["pins"][0]["ready"],
            got == newer
        );
        stale += usize::from(got != newer);
        lab.restore("b", Links::Both)?;

        command("unpin", &root_b)?;
        command("pin", &root_b)?;
        wait_ready(&lab)?;
        lab.cut("b", Links::Both)?;
        ensure!(
            fs::read(&path_b)? == newer,
            "trial {trial} refreshed-ready control was not the latest version"
        );
        lab.restore("b", Links::Both)?;
    }
    assert_eq!(stale, 0, "ready pins that served an older version offline, of 3");
    Ok(())
}

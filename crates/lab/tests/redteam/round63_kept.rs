//! Round 63: a ready kept directory must notice newly added descendants.

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

/// An existing child controls three children added after the directory became ready. Once the
/// new commit is visible, ready may not remain true unless that child is also readable offline.
// contract: P4, C2, C12
#[test]
#[ignore = "ROUND63: a ready kept folder omits children created after its last full scan"]
fn round63_kept_ready_includes_new_descendants() -> Result<()> {
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
    let root_a = lab.mount_path("a").join("kept-new");
    let root_b = lab.mount_path("b").join("kept-new");
    fs::create_dir(&root_a)?;
    let control = random_bytes(512 << 10);
    save(&root_a.join("control"), &control)?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    command("pin", &root_b)?;
    wait_ready(&lab)?;

    lab.cut("b", Links::Both)?;
    ensure!(
        fs::read(root_b.join("control"))? == control,
        "ready control was not offline"
    );
    lab.restore("b", Links::Both)?;

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("added-{trial}");
        let expected = random_bytes(512 << 10);
        save(&root_a.join(&name), &expected)?;
        lab.wait_committed("a")?;
        thread::sleep(VISIBLE_AFTER);
        let before = status(&lab)?;
        lab.cut("b", Links::Both)?;
        let got = fs::read(root_b.join(&name));
        let offline = got.as_ref().is_ok_and(|bytes| bytes == &expected);
        eprintln!(
            "trial {trial}: ready={}, new child offline={offline}, result={:?}",
            before["kept"]["pins"][0]["ready"],
            got.as_ref().map(Vec::len)
        );
        failures += usize::from(before["kept"]["pins"][0]["ready"] == true && !offline);
        lab.restore("b", Links::Both)?;
    }
    assert_eq!(failures, 0, "ready pins missing newly committed children, of 3");
    Ok(())
}

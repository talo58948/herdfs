//! Round 96: cancellation after an accepted queued prompt disappears.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const HOST: &str = "box";

struct Runner(Child);

impl Runner {
    fn start(mount: &Path) -> Self {
        let command = "while [ ! -f \"$HERDFS_SESSION_DIR/release\" ]; do sleep 0.05; done";
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", HOST, "--command", command])
            .args(["--poll-ms", "20", "--max-sessions", "1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn session(mount: &Path, name: &str) -> PathBuf {
    let dir = mount.join(".agents").join(HOST).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), "run\n").unwrap();
    dir
}

fn status(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("status"))
        .ok()
        .map(|status| status.trim().to_owned())
        .filter(|status| !status.is_empty())
}

fn wait_status(dir: &Path, wanted: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while status(dir).as_deref() != Some(wanted) {
        assert!(
            Instant::now() < deadline,
            "{} never became {wanted}",
            dir.display()
        );
        sleep(Duration::from_millis(20));
    }
}

/// Once the runner has accepted a prompt and marked its session queued, a later cancel must reach
/// that session even if the prompt disappears; queued sessions with intact prompts are controls.
// contract: A2
#[test]
#[ignore = "ROUND96: queued cancellation is skipped after the prompt disappears"]
fn queued_cancel_survives_prompt_removal() {
    let mount = tempfile::tempdir().unwrap();
    let blocker = session(mount.path(), "blocker");
    let _runner = Runner::start(mount.path());
    wait_status(&blocker, "running");

    let attacks: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("attack-{trial}")))
        .collect();
    let controls: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("control-{trial}")))
        .collect();
    for dir in attacks.iter().chain(&controls) {
        wait_status(dir, "queued");
    }
    for dir in &attacks {
        fs::remove_file(dir.join("prompt")).unwrap();
    }
    let cancelled = Instant::now();
    for dir in attacks.iter().chain(&controls) {
        fs::write(dir.join("cancel"), "").unwrap();
    }
    for dir in &controls {
        wait_status(dir, "cancelled");
        assert!(cancelled.elapsed() < Duration::from_secs(5));
    }
    sleep(Duration::from_secs(5));
    let stuck = attacks
        .iter()
        .filter(|dir| status(dir).as_deref() == Some("queued"))
        .count();

    for dir in &attacks {
        fs::write(dir.join("prompt"), "run\n").unwrap();
        wait_status(dir, "cancelled");
    }
    fs::write(blocker.join("cancel"), "").unwrap();
    wait_status(&blocker, "cancelled");

    assert_eq!(stuck, 0, "{stuck}/3 queued sessions ignored cancellation");
}

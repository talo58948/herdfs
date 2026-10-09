//! Round 58: cancellation while a session is waiting for a runner slot.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct Runner(Child);

impl Runner {
    fn start(mount: &Path) -> Self {
        let command = concat!("while [ ! -f \"$HERDFS_SESSION_DIR/release\" ]; do sleep 0.05; done");
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", "box", "--command", command])
            .args(["--poll-ms", "20", "--max-sessions", "1"])
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
    let dir = mount.join(".agents/box").join(name);
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

/// A cancel file stops queued sessions within five seconds as it does a running control session.
// contract: A2
#[test]
fn cancel_stops_sessions_that_are_waiting_for_a_slot() {
    let mount = TempDir::new().unwrap();
    let control = session(mount.path(), "control");
    let _runner = Runner::start(mount.path());
    wait_status(&control, "running");
    let cancelled = Instant::now();
    fs::write(control.join("cancel"), "").unwrap();
    wait_status(&control, "cancelled");
    assert!(
        cancelled.elapsed() < Duration::from_secs(5),
        "running control missed A2"
    );

    let blocker = session(mount.path(), "blocker");
    wait_status(&blocker, "running");
    let queued: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("queued-{trial}")))
        .collect();
    for dir in &queued {
        wait_status(dir, "queued");
        fs::write(dir.join("cancel"), "").unwrap();
    }
    sleep(Duration::from_millis(5_500));
    let stopped = queued
        .iter()
        .filter(|dir| status(dir).as_deref() == Some("cancelled"))
        .count();

    fs::write(blocker.join("release"), "").unwrap();
    wait_status(&blocker, "done");
    for dir in &queued {
        wait_status(dir, "cancelled");
    }
    assert_eq!(
        stopped,
        3,
        "{}/3 queued sessions exceeded A2's five-second bound",
        3 - stopped
    );
}

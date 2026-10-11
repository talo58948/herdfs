//! Round 98: queued cancellation under a crowded agent host directory.
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
            .args(["--poll-ms", "1", "--max-sessions", "1"])
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

fn wait_status(dir: &Path, wanted: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while status(dir).as_deref() != Some(wanted) {
        assert!(
            Instant::now() < deadline,
            "{} never became {wanted}",
            dir.display()
        );
        sleep(Duration::from_millis(20));
    }
}

fn cancel_queued(mount: &Path, crowded: bool) -> usize {
    let blocker = session(mount, "000-blocker");
    let _runner = Runner::start(mount);
    wait_status(&blocker, "running", Duration::from_secs(10));

    if crowded {
        let host = mount.join(".agents").join(HOST);
        for number in 0..100_000 {
            fs::create_dir(host.join(format!("filler-{number:06}"))).unwrap();
        }
    }
    let queued: Vec<_> = (0..3)
        .map(|trial| session(mount, &format!("zz-queued-{trial}")))
        .collect();
    for dir in &queued {
        wait_status(dir, "queued", Duration::from_secs(60));
    }

    let cancelled = Instant::now();
    for dir in &queued {
        fs::write(dir.join("cancel"), "").unwrap();
    }
    sleep(Duration::from_millis(5_200));
    let stopped = queued
        .iter()
        .filter(|dir| status(dir).as_deref() == Some("cancelled"))
        .count();
    eprintln!(
        "crowded={crowded}: {stopped}/3 cancelled in {:?}",
        cancelled.elapsed()
    );
    stopped
}

/// A queued cancel must be observed within five seconds even when other session directories are
/// present; queued sessions on an uncrowded host are the passing control.
// contract: A1, A2
#[test]
fn crowded_host_does_not_starve_queued_cancellation() {
    let control = tempfile::tempdir().unwrap();
    assert_eq!(cancel_queued(control.path(), false), 3, "uncrowded control");

    let attack = tempfile::tempdir().unwrap();
    let stopped = cancel_queued(attack.path(), true);
    assert_eq!(
        stopped,
        3,
        "{}/3 queued cancels exceeded five seconds",
        3 - stopped
    );
}

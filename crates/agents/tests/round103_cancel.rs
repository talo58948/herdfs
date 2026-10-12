//! Round 103: cancellation latency when many accepted sessions are queued together.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const HOST: &str = "box";
const QUEUED: usize = 1_000;

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

fn wait_queued(dirs: &[PathBuf]) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let ready = dirs
            .iter()
            .filter(|dir| status(dir).as_deref() == Some("queued"))
            .count();
        if ready == dirs.len() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "only {ready}/{} sessions queued",
            dirs.len()
        );
        sleep(Duration::from_millis(50));
    }
}

fn write_cancels(dirs: &[PathBuf]) {
    for dir in dirs {
        fs::write(dir.join("cancel"), "").unwrap();
    }
}

fn cancel_control(dirs: &[PathBuf]) {
    write_cancels(dirs);
    let deadline = Instant::now() + Duration::from_secs(5);
    while dirs.iter().any(|dir| status(dir).as_deref() != Some("cancelled")) {
        assert!(
            Instant::now() < deadline,
            "small-queue cancellation exceeded five seconds"
        );
        sleep(Duration::from_millis(20));
    }
}

fn delayed_after_deadline(dirs: &[PathBuf]) -> usize {
    write_cancels(dirs);
    sleep(Duration::from_millis(5_200));
    dirs.iter()
        .filter(|dir| status(dir).as_deref() != Some("cancelled"))
        .count()
}

fn trial(number: usize) -> usize {
    let mount = tempfile::tempdir().unwrap();
    let blocker = session(mount.path(), "000-blocker");
    let _runner = Runner::start(mount.path());
    wait_status(&blocker, "running", Duration::from_secs(10));

    let controls: Vec<_> = (0..3)
        .map(|index| session(mount.path(), &format!("control-{index}")))
        .collect();
    wait_queued(&controls);
    cancel_control(&controls);

    let queued: Vec<_> = (0..QUEUED)
        .map(|index| session(mount.path(), &format!("queued-{index:04}")))
        .collect();
    wait_queued(&queued);
    let delayed = delayed_after_deadline(&queued);
    eprintln!("trial {number}: {delayed}/{QUEUED} queued cancels exceeded five seconds");

    fs::write(blocker.join("cancel"), "").unwrap();
    wait_status(&blocker, "cancelled", Duration::from_secs(5));
    delayed
}

/// Every accepted queued session must observe its cancel within five seconds even when many other
/// accepted sessions are cancelled at once; three-session queues are the passing controls.
// contract: A2
#[test]
#[ignore = "ROUND103: serial queued cancellation exceeds five seconds under accepted load"]
fn simultaneous_queued_cancels_meet_the_five_second_bound() {
    let delayed: Vec<_> = (0..3).map(trial).collect();
    assert!(
        delayed.iter().all(|count| *count == 0),
        "delayed queued sessions in three trials: {delayed:?}"
    );
}

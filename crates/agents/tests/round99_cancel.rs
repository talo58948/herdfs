//! Round 99: queued cancellation while a restarted runner recovers a crowded host.
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
            .args(["--host", HOST, "--command", command, "--poll-ms", "1"])
            .args(["--max-sessions", "1"])
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

fn queued(mount: &Path, name: &str) -> PathBuf {
    let dir = mount.join(".agents").join(HOST).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), "run\n").unwrap();
    fs::write(dir.join("status"), "queued\n").unwrap();
    dir
}

fn cancelled(dir: &Path) -> bool {
    fs::read_to_string(dir.join("status")).is_ok_and(|status| status.trim() == "cancelled")
}

fn restart_cancel(mount: &Path, crowded: bool) -> usize {
    let host = mount.join(".agents").join(HOST);
    fs::create_dir_all(&host).unwrap();
    if crowded {
        for number in 0..100_000 {
            fs::create_dir(host.join(format!("filler-{number:06}"))).unwrap();
        }
    }
    let sessions: Vec<_> = (0..3)
        .map(|trial| queued(mount, &format!("zz-queued-{trial}")))
        .collect();
    let _runner = Runner::start(mount);
    sleep(Duration::from_millis(100));
    let started = Instant::now();
    for dir in &sessions {
        fs::write(dir.join("cancel"), "").unwrap();
    }
    sleep(Duration::from_millis(5_200));
    let stopped = sessions.iter().filter(|dir| cancelled(dir)).count();
    eprintln!(
        "crowded={crowded}: {stopped}/3 cancelled in {:?}",
        started.elapsed()
    );
    stopped
}

/// Startup recovery must not postpone queued cancellation beyond five seconds; an uncrowded
/// restart controls the same pre-existing queued-session path.
// contract: A2
#[test]
#[ignore = "ROUND99: startup recovery delays queued cancellation beyond five seconds"]
fn startup_recovery_does_not_starve_queued_cancellation() {
    let control = tempfile::tempdir().unwrap();
    assert_eq!(restart_cancel(control.path(), false), 3, "uncrowded control");

    let attack = tempfile::tempdir().unwrap();
    let stopped = restart_cancel(attack.path(), true);
    assert_eq!(
        stopped,
        3,
        "{}/3 queued cancels exceeded five seconds during recovery",
        3 - stopped
    );
}

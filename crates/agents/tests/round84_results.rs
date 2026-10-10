//! Round 84: result files for sessions cancelled before they start.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

struct Runner(Child);

impl Runner {
    fn start(mount: &Path) -> Self {
        let command = "while [ ! -f \"$HERDFS_SESSION_DIR/release\" ]; do sleep 0.05; done";
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", "box", "--command", command])
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

fn result_files(dir: &Path) -> usize {
    ["exit_code", "stdout", "stderr", "output"]
        .into_iter()
        .filter(|name| dir.join(name).is_file())
        .count()
}

/// Every terminal session must have the promised result files, including one cancelled while
/// queued; running cancellations are the control for the ordinary result-writing path.
// contract: A2
#[test]
#[ignore = "ROUND84: queued cancellations omit every promised result file"]
fn queued_cancellations_write_result_files() {
    let mount = tempfile::tempdir().unwrap();
    let control = session(mount.path(), "running-control");
    let _runner = Runner::start(mount.path());
    wait_status(&control, "running");
    fs::write(control.join("cancel"), "").unwrap();
    wait_status(&control, "cancelled");
    assert_eq!(result_files(&control), 4, "running-cancel control");

    let blocker = session(mount.path(), "blocker");
    wait_status(&blocker, "running");
    let queued: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("queued-{trial}")))
        .collect();
    for dir in &queued {
        wait_status(dir, "queued");
        fs::write(dir.join("cancel"), "").unwrap();
        wait_status(dir, "cancelled");
    }

    let complete = queued.iter().filter(|dir| result_files(dir) == 4).count();
    fs::write(blocker.join("release"), "").unwrap();
    wait_status(&blocker, "done");
    assert_eq!(
        complete,
        3,
        "{}/3 queued cancellations omitted result files",
        3 - complete
    );
}

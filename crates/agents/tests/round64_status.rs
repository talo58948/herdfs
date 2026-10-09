//! Round 64: recovery from a crash between truncating and writing a session status.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct Runner(Child);

impl Runner {
    fn start(mount: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", "box", "--command", "cat", "--poll-ms", "20"])
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

fn session(mount: &Path, name: &str, status: Option<&str>) -> PathBuf {
    let dir = mount.join(".agents/box").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), format!("{name}\n")).unwrap();
    if let Some(status) = status {
        fs::write(dir.join("status"), status).unwrap();
    }
    dir
}

fn status(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("status"))
        .ok()
        .map(|status| status.trim().to_owned())
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

/// An interrupted in-place status transition can leave the file empty. Restart recovery must
/// resume or fail those sessions instead of leaving a state outside A2 forever.
// contract: A2
#[test]
fn interrupted_status_writes_recover_to_a_valid_state() {
    let mount = TempDir::new().unwrap();
    let clean = session(mount.path(), "clean-control", None);
    let running = session(mount.path(), "running-control", Some("running\n"));
    let attacks: Vec<_> = ["", "invalid", "failed: "]
        .into_iter()
        .enumerate()
        .map(|(trial, status)| session(mount.path(), &format!("attack-{trial}"), Some(status)))
        .collect();
    let _runner = Runner::start(mount.path());

    wait_status(&clean, "done");
    wait_status(&running, "failed: runner restarted");
    for dir in &attacks {
        wait_status(dir, "failed: runner restarted");
    }
}

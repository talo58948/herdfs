//! Round 66: result capture after the tracked shell exits before its process group.
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
        let command = "echo immediate; (sleep 2; echo delayed) & exit 0";
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", "box", "--command", command, "--poll-ms", "20"])
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

fn session(mount: &Path, trial: usize) -> PathBuf {
    let dir = mount.join(format!(".agents/box/session-{trial}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), "run\n").unwrap();
    dir
}

fn wait_done(dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::read_to_string(dir.join("status")).is_ok_and(|status| status.trim() == "done") {
            return;
        }
        assert!(Instant::now() < deadline, "{} did not finish", dir.display());
        sleep(Duration::from_millis(20));
    }
}

/// A session is not done while members of its process group can still produce result bytes; the
/// immediate line controls that capture started normally before the tracked shell exited.
// contract: A2
#[test]
#[ignore = "ROUND66: done sessions discard late output from their still-running process group"]
fn done_waits_for_process_group_output() {
    let mount = TempDir::new().unwrap();
    let dirs: Vec<_> = (0..3).map(|trial| session(mount.path(), trial)).collect();
    let _runner = Runner::start(mount.path());

    for dir in &dirs {
        wait_done(dir);
    }
    sleep(Duration::from_millis(2_500));

    let mut lost = 0;
    for (trial, dir) in dirs.iter().enumerate() {
        let stdout = fs::read_to_string(dir.join("stdout")).unwrap();
        assert!(
            stdout.contains("immediate\n"),
            "capture control failed, trial {trial}"
        );
        lost += usize::from(!stdout.contains("delayed\n"));
    }
    assert_eq!(lost, 0, "{lost}/3 sessions lost process-group output");
}

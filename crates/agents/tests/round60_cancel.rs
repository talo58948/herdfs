//! Round 60: cancellation of descendants that leave the runner's process group.
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
        let command = concat!(
            "sleep 30 & echo $! > \"$HERDFS_SESSION_DIR/group.pid\"; ",
            "setsid sleep 30 </dev/null >/dev/null 2>&1 & ",
            "echo $! > \"$HERDFS_SESSION_DIR/detached.pid\"; wait"
        );
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

fn wait_text(path: &Path, wanted: Option<&str>) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            let text = text.trim().to_owned();
            if wanted.is_none_or(|wanted| text == wanted) {
                return text;
            }
        }
        assert!(Instant::now() < deadline, "{} was not ready", path.display());
        sleep(Duration::from_millis(20));
    }
}

fn process_exists(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn kill(pid: &str) {
    let _ = Command::new("kill")
        .args(["-KILL", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Cancelling a session must stop both an ordinary child and one that created a new session;
/// the ordinary child is the passing process-group control.
// contract: A2
#[test]
fn cancel_stops_descendants_that_leave_the_process_group() {
    let mut survivors = 0;
    for trial in 0..3 {
        let mount = TempDir::new().unwrap();
        let dir = session(mount.path(), trial);
        let _runner = Runner::start(mount.path());
        wait_text(&dir.join("status"), Some("running"));
        let group = wait_text(&dir.join("group.pid"), None);
        let detached = wait_text(&dir.join("detached.pid"), None);
        assert!(process_exists(&group), "group control never started, trial {trial}");
        assert!(process_exists(&detached), "detached child never started, trial {trial}");

        let cancelled = Instant::now();
        fs::write(dir.join("cancel"), "").unwrap();
        wait_text(&dir.join("status"), Some("cancelled"));
        assert!(cancelled.elapsed() < Duration::from_secs(5), "trial {trial}");
        assert!(!process_exists(&group), "group control survived, trial {trial}");
        survivors += usize::from(process_exists(&detached));
        kill(&detached);
    }
    assert_eq!(survivors, 0, "{survivors}/3 detached descendants survived cancellation");
}

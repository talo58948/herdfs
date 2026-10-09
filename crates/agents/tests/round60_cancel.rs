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
        Self::with_command(mount, command, "20", "2")
    }

    fn with_command(mount: &Path, command: &str, poll_ms: &str, max_sessions: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args([
                "--host",
                "box",
                "--command",
                command,
                "--poll-ms",
                poll_ms,
                "--max-sessions",
                max_sessions,
            ])
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
    let deadline = Instant::now() + Duration::from_secs(20);
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
        assert!(
            process_exists(&group),
            "group control never started, trial {trial}"
        );
        assert!(
            process_exists(&detached),
            "detached child never started, trial {trial}"
        );

        let cancelled = Instant::now();
        fs::write(dir.join("cancel"), "").unwrap();
        wait_text(&dir.join("status"), Some("cancelled"));
        assert!(cancelled.elapsed() < Duration::from_secs(5), "trial {trial}");
        assert!(!process_exists(&group), "group control survived, trial {trial}");
        survivors += usize::from(process_exists(&detached));
        kill(&detached);
    }
    assert_eq!(
        survivors, 0,
        "{survivors}/3 detached descendants survived cancellation"
    );
}

/// The configurable scan interval cannot weaken A2's five-second bound for queued sessions; a
/// running session, whose cancel watcher is independent of that interval, is the passing control.
// contract: A2
#[test]
fn cancel_stops_queued_sessions_with_a_slow_scan_interval() {
    let mount = TempDir::new().unwrap();
    let blocker = session(mount.path(), 100);
    let _runner = Runner::with_command(mount.path(), "sleep 30", "6000", "1");
    wait_text(&blocker.join("status"), Some("running"));
    let queued: Vec<_> = (0..3).map(|trial| session(mount.path(), trial)).collect();
    for dir in &queued {
        wait_text(&dir.join("status"), Some("queued"));
        fs::write(dir.join("cancel"), "").unwrap();
    }

    sleep(Duration::from_millis(5_200));
    let stopped = queued
        .iter()
        .filter(|dir| fs::read_to_string(dir.join("status")).is_ok_and(|status| status.trim() == "cancelled"))
        .count();

    let control_started = Instant::now();
    fs::write(blocker.join("cancel"), "").unwrap();
    wait_text(&blocker.join("status"), Some("cancelled"));
    assert!(control_started.elapsed() < Duration::from_secs(5));
    assert_eq!(stopped, 3, "{}/3 queued sessions exceeded A2", 3 - stopped);
}

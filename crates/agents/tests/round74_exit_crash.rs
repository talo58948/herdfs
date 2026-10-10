//! Round 74: recovery after a command leader exits just before the runner crashes.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const HOST: &str = "box";
const COMMAND: &str = "(trap '' HUP; while :; do sleep 1; done) & \
    printf '%s\n' \"$$\" > \"$HERDFS_SESSION_DIR/leader\"; \
    printf '%s\n' \"$!\" > \"$HERDFS_SESSION_DIR/member\"; \
    while [ ! -e \"$HERDFS_SESSION_DIR/go\" ]; do sleep 0.01; done";

struct Runner(Child);

impl Runner {
    fn start(mount: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", HOST, "--command", COMMAND])
            .args(["--poll-ms", "20", "--max-sessions", "3"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
    }

    fn stop(&self) {
        signal("-STOP", &self.0.id().to_string());
    }

    fn crash(mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sessions(mount: &Path, prefix: &str) -> Vec<PathBuf> {
    (0..3)
        .map(|trial| {
            let dir = mount.join(".agents").join(HOST).join(format!("{prefix}-{trial}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("prompt"), "run\n").unwrap();
            dir
        })
        .collect()
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
}

fn wait_status(dirs: &[PathBuf], wanted: &str) {
    wait(wanted, || {
        dirs.iter()
            .all(|dir| read(&dir.join("status")).as_deref() == Some(wanted))
    });
}

fn pids(dirs: &[PathBuf], name: &str) -> Vec<String> {
    let mut pids = Vec::new();
    wait(name, || {
        pids = dirs.iter().filter_map(|dir| read(&dir.join(name))).collect();
        pids.len() == dirs.len()
    });
    pids
}

fn exists(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn zombie(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.contains(") Z "))
}

fn signal(kind: &str, pid: &str) {
    let _ = Command::new("kill")
        .args([kind, pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Restart recovery must kill a recorded process group's surviving members even when its command
/// leader exited just before the runner crashed; ordinary cancellation is the passing control.
// contract: A2
#[test]
fn recovery_contains_a_group_after_its_leader_exits() {
    let mount = TempDir::new().unwrap();
    let attacks = sessions(mount.path(), "attack");
    let runner = Runner::start(mount.path());
    wait_status(&attacks, "running");
    let leaders = pids(&attacks, "leader");
    let members = pids(&attacks, "member");

    runner.stop();
    for dir in &attacks {
        fs::write(dir.join("go"), "").unwrap();
    }
    wait("command leaders to exit", || {
        leaders.iter().all(|pid| zombie(pid))
    });
    runner.crash();
    wait("command leaders to be reaped", || {
        leaders.iter().all(|pid| !exists(pid))
    });

    let replacement = Runner::start(mount.path());
    wait_status(&attacks, "failed: runner restarted");
    let survived = members.iter().filter(|pid| exists(pid)).count();
    for pid in &members {
        signal("-KILL", pid);
    }

    let controls = sessions(mount.path(), "control");
    wait_status(&controls, "running");
    let control_members = pids(&controls, "member");
    for dir in &controls {
        fs::write(dir.join("cancel"), "").unwrap();
    }
    wait_status(&controls, "cancelled");
    let control_survived = control_members.iter().filter(|pid| exists(pid)).count();
    eprintln!("recovery: {survived}/3 members survived; cancellation control: {control_survived}/3");
    assert_eq!(
        control_survived, 0,
        "ordinary cancellation left process-group members alive"
    );
    drop(replacement);

    assert_eq!(survived, 0, "{survived}/3 recovered group members survived");
}

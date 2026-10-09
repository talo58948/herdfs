//! Round 68: process containment across a runner crash and restart.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const HOST: &str = "box";

struct Runner(Child);

impl Runner {
    fn start(mount: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", HOST])
            .args(["--command", "echo $$; exec sleep 30"])
            .args(["--poll-ms", "20", "--max-sessions", "3"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
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

fn pids(dirs: &[PathBuf]) -> Vec<String> {
    let mut pids = Vec::new();
    wait("command pids", || {
        pids = dirs.iter().filter_map(|dir| read(&dir.join("stdout"))).collect();
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

fn kill(pid: &str) {
    let _ = Command::new("kill")
        .args(["-KILL", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Recovery must not leave a command alive behind terminal `failed` status where a cancel file
/// can no longer reach it; ordinary running cancellation is the control.
// contract: A2
#[test]
#[ignore = "ROUND68: restarted recovery strands live commands behind terminal failed status"]
fn restart_recovery_stops_the_previous_process_groups() {
    let mount = TempDir::new().unwrap();
    let attacks = sessions(mount.path(), "attack");
    let runner = Runner::start(mount.path());
    wait_status(&attacks, "running");
    let attack_pids = pids(&attacks);
    runner.crash();

    let replacement = Runner::start(mount.path());
    wait_status(&attacks, "failed: runner restarted");
    for dir in &attacks {
        fs::write(dir.join("cancel"), "").unwrap();
    }
    sleep(Duration::from_millis(5_200));
    let survived = attack_pids.iter().filter(|pid| exists(pid)).count();
    for pid in &attack_pids {
        kill(pid);
    }

    let controls = sessions(mount.path(), "control");
    wait_status(&controls, "running");
    let control_pids = pids(&controls);
    for dir in &controls {
        fs::write(dir.join("cancel"), "").unwrap();
    }
    wait_status(&controls, "cancelled");
    assert!(
        control_pids.iter().all(|pid| !exists(pid)),
        "ordinary cancellation control left a command alive"
    );
    drop(replacement);

    assert_eq!(survived, 0, "{survived}/3 recovered commands survived cancel");
}

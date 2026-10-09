//! Round 72: process containment when a running session directory is renamed.
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
            .args(["--host", "box", "--command", "echo $$; exec sleep 30"])
            .args(["--poll-ms", "20", "--max-sessions", "4"])
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

fn session(mount: &Path, name: &str) -> PathBuf {
    let dir = mount.join(".agents/box").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), "run\n").unwrap();
    dir
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

fn pid(dir: &Path) -> String {
    let mut pid = None;
    wait("command pid", || {
        pid = read(&dir.join("stdout"));
        pid.is_some()
    });
    pid.unwrap()
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

/// Recovery identifies a session by its held directory, so renaming that directory must not
/// strand its live process group behind the replacement runner's terminal failure status.
// contract: A2
#[test]
#[ignore = "ROUND72: renamed sessions evade crash recovery and strand their process groups"]
fn renamed_running_sessions_remain_contained_across_restart() {
    let mount = TempDir::new().unwrap();
    let control = session(mount.path(), "control");
    let attacks: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("attack-{trial}")))
        .collect();
    let runner = Runner::start(mount.path());
    let control_pid = pid(&control);
    let attack_pids: Vec<_> = attacks.iter().map(|dir| pid(dir)).collect();
    let renamed: Vec<_> = attacks
        .iter()
        .enumerate()
        .map(|(trial, dir)| {
            let destination = mount.path().join(format!(".agents/box/renamed-{trial}"));
            fs::rename(dir, &destination).unwrap();
            destination
        })
        .collect();
    runner.crash();

    let _replacement = Runner::start(mount.path());
    wait("recovery statuses", || {
        std::iter::once(&control)
            .chain(&renamed)
            .all(|dir| read(&dir.join("status")).as_deref() == Some("failed: runner restarted"))
    });
    wait("unrenamed control process to stop", || {
        !process_exists(&control_pid)
    });
    sleep(Duration::from_millis(200));
    let survived = attack_pids.iter().filter(|pid| process_exists(pid)).count();
    for pid in &attack_pids {
        kill(pid);
    }

    assert_eq!(
        survived, 0,
        "{survived}/3 renamed process groups survived recovery"
    );
}

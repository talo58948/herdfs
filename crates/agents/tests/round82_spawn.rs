//! Round 82: process containment across the spawn-to-record crash window.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct Runner(Child);

impl Runner {
    fn start(mount: &Path, runtime: &Path, command: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", "box", "--command", command, "--poll-ms", "20"])
            .env("XDG_RUNTIME_DIR", runtime)
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
        sleep(Duration::from_millis(10));
    }
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

fn kill_group(pid: &str) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn trial(stopping: bool, number: usize) -> bool {
    let mount = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let dir = session(mount.path(), &format!("session-{number}"));
    let command = if stopping {
        "kill -STOP \"$PPID\"; echo $$ > \"$HERDFS_SESSION_DIR/pid\"; exec sleep 30"
    } else {
        "echo $$ > \"$HERDFS_SESSION_DIR/pid\"; exec sleep 30"
    };
    let runner = Runner::start(mount.path(), runtime.path(), command);
    wait("running command", || read(&dir.join("pid")).is_some());
    let pid = read(&dir.join("pid")).unwrap();
    if !stopping {
        wait("durable process record", || {
            runtime
                .path()
                .join("herdfs-agents")
                .read_dir()
                .is_ok_and(|mut entries| entries.next().is_some())
        });
    }
    runner.crash();

    let replacement = Runner::start(mount.path(), runtime.path(), "exit 0");
    wait("recovery status", || {
        read(&dir.join("status")).as_deref() == Some("failed: runner restarted")
    });
    sleep(Duration::from_millis(100));
    let survived = exists(&pid);
    kill_group(&pid);
    drop(replacement);
    survived
}

/// A command that stops the runner immediately after spawn must remain recoverable after the
/// runner crashes; crashing after the process-group record is durable is the passing control.
// contract: A2
#[test]
#[ignore = "ROUND82: a command can stop the runner before its process group is recorded"]
fn recovery_contains_a_command_that_stops_the_runner_before_recording() {
    let mut survived = 0;
    for trial_number in 0..3 {
        survived += usize::from(trial(true, trial_number));
        assert!(
            !trial(false, trial_number + 10),
            "control {trial_number} survived"
        );
    }
    assert_eq!(survived, 0, "{survived}/3 commands survived recovery");
}

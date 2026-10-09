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
        let command = "(printf 'stdout kept\\n'; printf 'stderr kept\\n' >&2; \
                       touch \"$HERDFS_SESSION_DIR/ready\"; sleep 30) & \
                       printf '%s\\n' \"$!\" > \"$HERDFS_SESSION_DIR/leftover_pid\"; \
                       while [ ! -e \"$HERDFS_SESSION_DIR/ready\" ]; do sleep 0.01; done";
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

fn process_exists(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Output written before the command exits is retained, and a group member left sleeping by the
/// command is killed before the session becomes done.
// contract: A2
#[test]
fn command_exit_keeps_written_output_and_kills_the_process_group() {
    let mount = TempDir::new().unwrap();
    let dirs: Vec<_> = (0..3).map(|trial| session(mount.path(), trial)).collect();
    let _runner = Runner::start(mount.path());

    for dir in &dirs {
        wait_done(dir);
    }
    for (trial, dir) in dirs.iter().enumerate() {
        let stdout = fs::read_to_string(dir.join("stdout")).unwrap();
        let stderr = fs::read_to_string(dir.join("stderr")).unwrap();
        let output = fs::read_to_string(dir.join("output")).unwrap();
        assert_eq!(stdout, "stdout kept\n", "lost stdout in trial {trial}");
        assert_eq!(stderr, "stderr kept\n", "lost stderr in trial {trial}");
        assert!(output.contains(&stdout), "lost combined stdout in trial {trial}");
        assert!(output.contains(&stderr), "lost combined stderr in trial {trial}");
        let pid = fs::read_to_string(dir.join("leftover_pid"))
            .unwrap()
            .trim()
            .to_owned();
        assert!(
            !process_exists(&pid),
            "leftover process {pid} survived trial {trial}"
        );
    }
}

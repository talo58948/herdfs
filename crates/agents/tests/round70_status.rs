//! Round 70: terminal session states rewritten through the public filesystem.
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
        let command = "echo run >> \"$HERDFS_SESSION_DIR/runs\"";
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

fn session(mount: &Path, name: &str) -> PathBuf {
    let dir = mount.join(".agents/box").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), format!("{name}\n")).unwrap();
    dir
}

fn executions(dir: &Path) -> usize {
    fs::read_to_string(dir.join("runs"))
        .unwrap_or_default()
        .lines()
        .count()
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
}

fn wait_done(dirs: &[PathBuf], runs: usize) {
    wait_for("sessions to finish", || {
        dirs.iter().all(|dir| {
            fs::read_to_string(dir.join("status")).is_ok_and(|status| status.trim() == "done")
                && executions(dir) == runs
        })
    });
}

/// Rewriting a completed session's ordinary status file to `queued` must not move its A2 state
/// backward or execute its prompt again; an untouched completed session is the control.
// contract: A2
#[test]
#[ignore = "outside A2: status rewritten by a principal who may spawn sessions (A1)"]
fn terminal_status_cannot_be_requeued() {
    let mount = TempDir::new().unwrap();
    let attacks: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("attack-{trial}")))
        .collect();
    let control = session(mount.path(), "control");
    let _runner = Runner::start(mount.path());

    let mut all = attacks.clone();
    all.push(control.clone());
    wait_done(&all, 1);
    for dir in &attacks {
        fs::write(dir.join("status"), "queued\n").unwrap();
    }
    wait_done(&attacks, 2);
    sleep(Duration::from_millis(200));

    let repeated = attacks.iter().filter(|dir| executions(dir) == 2).count();
    assert_eq!(executions(&control), 1, "untouched terminal control reran");
    assert_eq!(repeated, 0, "{repeated}/3 terminal sessions ran twice");
}

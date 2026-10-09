//! Round 62: completed session output contains only bytes produced by that run.
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

fn session(mount: &Path, name: &str, stale_output: bool) -> PathBuf {
    let dir = mount.join(".agents/box").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), format!("fresh-{name}\n")).unwrap();
    if stale_output {
        fs::write(dir.join("output"), "caller-supplied stale bytes\n").unwrap();
    }
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

/// A clean session controls three sessions whose caller pre-created `output`. Completing a run
/// must not preserve bytes that were not emitted by its process.
// contract: A2
#[test]
fn completed_output_excludes_caller_supplied_bytes() {
    let mount = TempDir::new().unwrap();
    let control = session(mount.path(), "control", false);
    let attacks: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("attack-{trial}"), true))
        .collect();
    let _runner = Runner::start(mount.path());

    wait_done(&control);
    assert_eq!(
        fs::read_to_string(control.join("output")).unwrap(),
        "fresh-control\n"
    );
    let mut polluted = 0;
    for (trial, dir) in attacks.iter().enumerate() {
        wait_done(dir);
        let output = fs::read_to_string(dir.join("output")).unwrap();
        let stdout = fs::read_to_string(dir.join("stdout")).unwrap();
        eprintln!("trial {trial}: output={output:?}, stdout={stdout:?}");
        polluted += usize::from(output != stdout);
    }
    assert_eq!(polluted, 0, "{polluted}/3 outputs retained caller bytes");
}

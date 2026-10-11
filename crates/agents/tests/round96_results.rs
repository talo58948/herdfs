//! Round 96: terminal results when a caller plants a directory at a result name.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const HOST: &str = "box";
const RESULTS: [&str; 4] = ["exit_code", "stdout", "stderr", "output"];

struct Runner(Child);

impl Runner {
    fn start(mount: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", HOST, "--command", "cat", "--poll-ms", "20"])
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

fn session(mount: &Path, name: &str, planted: bool) -> PathBuf {
    let dir = mount.join(".agents").join(HOST).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), "run\n").unwrap();
    fs::write(dir.join("cwd"), "../outside\n").unwrap();
    if planted {
        fs::create_dir(dir.join("output")).unwrap();
    }
    dir
}

fn wait_failed(dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::read_to_string(dir.join("status"))
            .is_ok_and(|status| status.trim() == "failed: cwd outside mount")
        {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for failure");
        sleep(Duration::from_millis(20));
    }
}

fn file_results(dir: &Path) -> usize {
    RESULTS.iter().filter(|name| dir.join(name).is_file()).count()
}

/// Every terminal session must have all four result files even when it is rejected before spawn;
/// identical rejected sessions without a planted result directory are the passing controls.
// contract: A2
#[test]
#[ignore = "ROUND96: a planted result directory leaves terminal sessions without all results"]
fn terminal_failure_replaces_result_directories() {
    let mount = tempfile::tempdir().unwrap();
    let controls: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("control-{trial}"), false))
        .collect();
    let attacks: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("attack-{trial}"), true))
        .collect();
    let _runner = Runner::start(mount.path());

    let mut incomplete = 0;
    for trial in 0..3 {
        wait_failed(&controls[trial]);
        wait_failed(&attacks[trial]);
        assert_eq!(file_results(&controls[trial]), RESULTS.len(), "control {trial}");
        let files = file_results(&attacks[trial]);
        eprintln!("trial {trial}: terminal session wrote {files}/4 result files");
        incomplete += usize::from(files != RESULTS.len());
    }
    assert_eq!(incomplete, 0, "{incomplete}/3 terminal sessions lacked results");
}

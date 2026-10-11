//! Round 90: result integrity when a session fails before its process starts.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::BTreeMap;
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

fn session(mount: &Path, name: &str, poisoned: bool) -> PathBuf {
    let dir = mount.join(".agents").join(HOST).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), "hello\n").unwrap();
    fs::write(dir.join("cwd"), "../outside\n").unwrap();
    if poisoned {
        for result in RESULTS {
            fs::write(dir.join(result), format!("caller supplied {result}\n")).unwrap();
        }
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
        assert!(Instant::now() < deadline, "{} did not fail", dir.display());
        sleep(Duration::from_millis(20));
    }
}

fn results(dir: &Path) -> BTreeMap<&'static str, Vec<u8>> {
    RESULTS
        .into_iter()
        .map(|name| (name, fs::read(dir.join(name)).unwrap()))
        .collect()
}

/// A pre-spawn failure must write the same results even when the caller pre-created their names;
/// clean failures are the controls for the runner-authored contents.
// contract: A2
#[test]
fn pre_spawn_failures_replace_caller_supplied_results() {
    let mount = tempfile::tempdir().unwrap();
    let controls: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("control-{trial}"), false))
        .collect();
    let attacks: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("attack-{trial}"), true))
        .collect();
    let _runner = Runner::start(mount.path());
    let mut polluted = 0;

    for trial in 0..3 {
        wait_failed(&controls[trial]);
        wait_failed(&attacks[trial]);
        let expected = results(&controls[trial]);
        let observed = results(&attacks[trial]);
        eprintln!("trial {trial}: expected={expected:?}, observed={observed:?}");
        polluted += usize::from(observed != expected);
    }

    assert_eq!(polluted, 0, "{polluted}/3 failures retained caller results");
}

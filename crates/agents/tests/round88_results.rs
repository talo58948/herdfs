//! Round 88: result files when a session fails before its process starts.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const HOST: &str = "box";

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

fn session(mount: &Path, name: &str, cwd: Option<&str>) -> PathBuf {
    let dir = mount.join(".agents").join(HOST).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), "hello\n").unwrap();
    if let Some(cwd) = cwd {
        fs::write(dir.join("cwd"), cwd).unwrap();
    }
    dir
}

fn status(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("status"))
        .ok()
        .map(|status| status.trim().to_owned())
}

fn wait_status(dir: &Path, wanted: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while status(dir).as_deref() != Some(wanted) {
        assert!(Instant::now() < deadline, "timed out waiting for {wanted}");
        sleep(Duration::from_millis(20));
    }
}

fn result_files(dir: &Path) -> usize {
    ["exit_code", "stdout", "stderr", "output"]
        .iter()
        .filter(|name| dir.join(name).is_file())
        .count()
}

/// A session that fails before spawn still needs A2's result files; successful sessions control
/// the ordinary result-writing path.
// contract: A2
#[test]
fn pre_spawn_failures_write_result_files() {
    let mount = tempfile::tempdir().unwrap();
    let _runner = Runner::start(mount.path());
    let mut complete = 0;

    for trial in 0..3 {
        let control = session(mount.path(), &format!("control-{trial}"), None);
        wait_status(&control, "done");
        assert_eq!(result_files(&control), 4, "control {trial}");

        let failed = session(mount.path(), &format!("failed-{trial}"), Some("../outside"));
        wait_status(&failed, "failed: cwd outside mount");
        let files = result_files(&failed);
        eprintln!("trial {trial}: pre-spawn failure wrote {files}/4 result files");
        complete += usize::from(files == 4);
    }

    assert_eq!(
        complete,
        3,
        "{}/3 pre-spawn failures omitted result files",
        3 - complete
    );
}

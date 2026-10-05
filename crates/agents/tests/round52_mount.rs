//! Round 52: the runner stays anchored to the canonical mount when its launch path changes.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct Runner(Child);

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

fn wait_for(path: &Path, wanted: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::read_to_string(path).is_ok_and(|text| text.trim() == wanted) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} never became {wanted}",
            path.display()
        );
        sleep(Duration::from_millis(20));
    }
}

/// Retargeting a symlink used as `--mount` cannot move the running scanner to another tree; a
/// session in the canonical original runs, while one in the replacement is untouched.
// contract: A1
#[test]
fn runner_stays_on_canonical_mount_after_launch_symlink_is_retargeted() {
    for trial in 0..3 {
        let root = TempDir::new().unwrap();
        let original = root.path().join(format!("original-{trial}"));
        let replacement = root.path().join(format!("replacement-{trial}"));
        let launch = root.path().join(format!("mount-{trial}"));
        fs::create_dir(&original).unwrap();
        fs::create_dir(&replacement).unwrap();
        symlink(&original, &launch).unwrap();
        let ready = session(&original, "ready");
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(&launch)
            .args(["--host", "box", "--command", "cat", "--poll-ms", "20"])
            .spawn()
            .unwrap();
        let _runner = Runner(child);
        wait_for(&ready.join("status"), "done");
        fs::remove_file(&launch).unwrap();
        symlink(&replacement, &launch).unwrap();
        let control = session(&original, "control");
        let attack = session(&replacement, "attack");

        wait_for(&control.join("status"), "done");
        sleep(Duration::from_millis(200));
        assert!(!attack.join("status").exists(), "trial {trial}");
    }
}

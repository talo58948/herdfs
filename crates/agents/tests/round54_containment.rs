//! Round 54: replacing the watched host directory with an outside symlink.
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

fn session(host: &Path, name: &str) -> PathBuf {
    let dir = host.join(name);
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

/// A host-directory symlink swapped in between polls cannot redirect session execution outside
/// the canonical mount; restoring the real directory and running there is the control.
// contract: A1
#[test]
fn a_raced_host_directory_symlink_cannot_escape_the_mount() {
    let mount = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let agents = mount.path().join(".agents");
    let host = agents.join("box");
    fs::create_dir_all(&host).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
        .arg("--mount")
        .arg(mount.path())
        .args(["--host", "box", "--command", "cat", "--poll-ms", "20"])
        .spawn()
        .unwrap();
    let _runner = Runner(child);

    for trial in 0..3 {
        let held = agents.join(format!("held-{trial}"));
        fs::rename(&host, &held).unwrap();
        symlink(outside.path(), &host).unwrap();
        let attack = session(outside.path(), &format!("attack-{trial}"));
        sleep(Duration::from_millis(500));
        assert!(!attack.join("status").exists(), "outside status in trial {trial}");
        assert!(!attack.join("output").exists(), "outside output in trial {trial}");

        fs::remove_file(&host).unwrap();
        fs::rename(&held, &host).unwrap();
        let control = session(&host, &format!("control-{trial}"));
        wait_for(&control.join("status"), "done");
    }
}

//! Session paths stay inside the public mount and refer only to regular files.

#![allow(clippy::unwrap_used, reason = "allow-unwrap-in-tests does not cover helpers")]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const HOST: &str = "box";

struct Runner(Child);

impl Runner {
    fn start(mount: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", HOST, "--command", "cat", "--poll-ms", "50"])
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
    let dir = mount.join(".agents").join(HOST).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), "hi").unwrap();
    dir
}

fn final_status(dir: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = fs::read_to_string(dir.join("status")).ok();
        if let Some(status) = status.filter(|status| !matches!(status.trim(), "" | "queued" | "running")) {
            return status.trim().to_owned();
        }
        assert!(Instant::now() < deadline, "session did not finish");
        sleep(Duration::from_millis(20));
    }
}

// contract: A1, A2
#[test]
fn refuses_session_files_that_are_not_regular_files() {
    let mount = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let secret = outside.path().join("secret");
    fs::write(&secret, "secret").unwrap();
    let prompt_link = session(mount.path(), "prompt-link");
    fs::remove_file(prompt_link.join("prompt")).unwrap();
    symlink(&secret, prompt_link.join("prompt")).unwrap();
    let output_link = session(mount.path(), "output-link");
    symlink(&secret, output_link.join("output")).unwrap();
    let stdout_link = session(mount.path(), "stdout-link");
    symlink(&secret, stdout_link.join("stdout")).unwrap();
    let cwd_link = session(mount.path(), "cwd-link");
    symlink(&secret, cwd_link.join("cwd")).unwrap();
    let output_dir = session(mount.path(), "output-dir");
    fs::create_dir(output_dir.join("output")).unwrap();
    let status_link = session(mount.path(), "status-link");
    symlink(outside.path().join("status"), status_link.join("status")).unwrap();
    fs::write(outside.path().join("prompt"), "hi").unwrap();
    symlink(
        outside.path(),
        mount.path().join(".agents").join(HOST).join("dir-link"),
    )
    .unwrap();
    let _runner = Runner::start(mount.path());

    for dir in [&prompt_link, &output_link, &stdout_link, &cwd_link, &output_dir] {
        assert_eq!(
            final_status(dir),
            "failed: not a regular file in the session",
            "{}",
            dir.display()
        );
    }
    assert_eq!(fs::read_to_string(&secret).unwrap(), "secret");
    assert!(prompt_link.join("output").is_file());
    assert!(!status_link.join("output").exists());
    assert!(!outside.path().join("status").exists());
    assert!(!outside.path().join("output").exists());
}

// contract: A1
#[test]
fn ignores_a_host_directory_reached_through_a_symlink() {
    let mount = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let dir = session(outside.path(), "s1");
    fs::create_dir(mount.path().join(".agents")).unwrap();
    symlink(
        outside.path().join(".agents").join(HOST),
        mount.path().join(".agents").join(HOST),
    )
    .unwrap();
    let _runner = Runner::start(mount.path());
    sleep(Duration::from_millis(500));
    assert!(!dir.join("status").exists());
    assert!(!dir.join("output").exists());
}

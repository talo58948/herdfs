//! Round 66: prompt writers that pause without closing the file.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs::{self, File, OpenOptions};
use std::io::Write;
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

fn session(mount: &Path, name: &str) -> PathBuf {
    let dir = mount.join(".agents/box").join(name);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn partial_prompt(dir: &Path) -> File {
    let mut prompt = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(dir.join("prompt"))
        .unwrap();
    prompt.write_all(b"first half\n").unwrap();
    prompt.flush().unwrap();
    prompt
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

/// A prompt is complete on close, not merely because its size and mtime stayed unchanged for two
/// polls; a prompt closed before discovery controls the normal complete-session path.
// contract: A1
#[test]
#[ignore = "outside A1: write prompt atomically; see docs/architecture.md"]
fn prompt_waits_for_its_writer_to_close() {
    let mount = TempDir::new().unwrap();
    let control = session(mount.path(), "control");
    fs::write(control.join("prompt"), "complete control\n").unwrap();
    let attacks: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("attack-{trial}")))
        .collect();
    let mut prompts: Vec<_> = attacks.iter().map(|dir| partial_prompt(dir)).collect();
    let _runner = Runner::start(mount.path());

    wait_done(&control);
    assert_eq!(
        fs::read_to_string(control.join("output")).unwrap(),
        "complete control\n"
    );
    for dir in &attacks {
        wait_done(dir);
    }
    for prompt in &mut prompts {
        prompt.write_all(b"second half\n").unwrap();
        prompt.sync_all().unwrap();
    }
    drop(prompts);

    let lost = attacks
        .iter()
        .filter(|dir| fs::read_to_string(dir.join("output")).unwrap() != "first half\nsecond half\n")
        .count();
    assert_eq!(lost, 0, "{lost}/3 sessions ran before prompt close");
}

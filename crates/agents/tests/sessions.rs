//! Runs `herdfs-agents` against a plain temp dir standing in for the mount.

#![allow(clippy::unwrap_used, reason = "allow-unwrap-in-tests does not cover helpers")]

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const HOST: &str = "box";

/// A runner serving `HOST`, killed when dropped.
struct Runner(Child);

impl Runner {
    fn start(mount: &Path, args: &[&str]) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args(["--host", HOST])
            .args(args)
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

/// Creates session `name` on `host` with its prompt, and returns its directory.
fn session(mount: &Path, host: &str, name: &str, prompt: &str) -> PathBuf {
    let dir = mount.join(".agents").join(host).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), prompt).unwrap();
    dir
}

/// A session file without its trailing newline; `None` while missing or mid-write.
fn read(dir: &Path, file: &str) -> Option<String> {
    let text = fs::read_to_string(dir.join(file)).ok()?;
    Some(text.trim_end().to_owned()).filter(|text| !text.is_empty())
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
}

/// Waits for the session to finish and returns its status.
fn final_status(dir: &Path) -> String {
    let mut status = None;
    wait_until("a final status", || {
        status = read(dir, "status").filter(|status| status != "running");
        status.is_some()
    });
    status.unwrap()
}

// contract: A1
#[test]
fn runs_a_session_to_done() {
    let mount = TempDir::new().unwrap();
    let dir = session(mount.path(), HOST, "s1", "hello\n");
    let _runner = Runner::start(mount.path(), &["--command", "cat; echo ok", "--poll-ms", "50"]);
    assert_eq!(final_status(&dir), "done");
    assert_eq!(read(&dir, "output").unwrap(), "hello\nok");
}

// contract: A1
#[test]
fn reports_the_exit_code_of_a_failing_command() {
    let mount = TempDir::new().unwrap();
    let dir = session(mount.path(), HOST, "s1", "hello\n");
    let _runner = Runner::start(
        mount.path(),
        &["--command", "echo oops >&2; exit 3", "--poll-ms", "50"],
    );
    assert_eq!(final_status(&dir), "failed: exit 3");
    assert_eq!(read(&dir, "output").unwrap(), "oops");
}

// contract: A1
#[test]
fn waits_until_the_prompt_stops_changing() {
    let mount = TempDir::new().unwrap();
    let _runner = Runner::start(mount.path(), &["--command", "cat", "--poll-ms", "300"]);
    let dir = session(mount.path(), HOST, "s1", "start");
    let mut prompt = fs::OpenOptions::new()
        .append(true)
        .open(dir.join("prompt"))
        .unwrap();
    let writing = Instant::now();
    while writing.elapsed() < Duration::from_secs(2) {
        prompt.write_all(b".").unwrap();
        assert!(
            !dir.join("status").exists(),
            "started while the prompt was being written"
        );
        sleep(Duration::from_millis(20));
    }
    prompt.write_all(b"end").unwrap();
    assert_eq!(final_status(&dir), "done");
    assert_eq!(read(&dir, "output"), read(&dir, "prompt"));
}

// contract: A1
#[test]
fn runs_at_most_max_sessions_at_once() {
    let mount = TempDir::new().unwrap();
    let dirs: Vec<_> = (0..5)
        .map(|n| session(mount.path(), HOST, &format!("s{n}"), ""))
        .collect();
    let args = ["--command", "sleep 0.5", "--poll-ms", "20", "--max-sessions", "2"];
    let _runner = Runner::start(mount.path(), &args);
    let mut most_running = 0;
    wait_until("every session done", || {
        let statuses: Vec<_> = dirs.iter().map(|dir| read(dir, "status")).collect();
        let running = statuses
            .iter()
            .filter(|status| status.as_deref() == Some("running"))
            .count();
        most_running = most_running.max(running);
        statuses.iter().all(|status| status.as_deref() == Some("done"))
    });
    assert_eq!(most_running, 2);
}

// contract: A1
#[test]
fn marks_sessions_left_running_as_failed_on_restart() {
    let mount = TempDir::new().unwrap();
    let dir = session(mount.path(), HOST, "s1", "hello\n");
    fs::write(dir.join("status"), "running\n").unwrap();
    let _runner = Runner::start(mount.path(), &["--command", "cat", "--poll-ms", "50"]);
    assert_eq!(final_status(&dir), "failed: runner restarted");
    assert!(!dir.join("output").exists());
}

// contract: A1
#[test]
fn runs_in_cwd_and_rejects_cwd_outside_the_mount() {
    let mount = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let root = mount.path().canonicalize().unwrap();
    fs::create_dir(root.join("work")).unwrap();
    symlink(outside.path(), root.join("escape")).unwrap();
    let inside = session(&root, HOST, "inside", "");
    fs::write(inside.join("cwd"), "work\n").unwrap();
    let escapes = [
        outside.path().to_str().unwrap(),
        "../..",
        "work/../work",
        "escape",
        "work/../escape",
    ];
    let escaping: Vec<_> = escapes
        .iter()
        .enumerate()
        .map(|(n, cwd)| {
            let dir = session(&root, HOST, &format!("escape{n}"), "");
            fs::write(dir.join("cwd"), cwd).unwrap();
            dir
        })
        .collect();
    let command = r#"pwd -P; echo "$HERDFS_MOUNT"; echo "$HERDFS_SESSION_DIR""#;
    let _runner = Runner::start(&root, &["--command", command, "--poll-ms", "50"]);

    assert_eq!(final_status(&inside), "done");
    let expected = format!(
        "{}\n{}\n{}",
        root.join("work").display(),
        root.display(),
        inside.display()
    );
    assert_eq!(read(&inside, "output").unwrap(), expected);
    for (dir, cwd) in escaping.iter().zip(escapes) {
        assert_eq!(final_status(dir), "failed: cwd outside mount", "cwd {cwd:?}");
        assert!(!dir.join("output").exists(), "cwd {cwd:?}");
    }
}

// contract: A1
#[test]
fn ignores_sessions_of_other_hosts() {
    let mount = TempDir::new().unwrap();
    let other = session(mount.path(), "other", "s1", "hello\n");
    let mine = session(mount.path(), HOST, "s1", "hello\n");
    let _runner = Runner::start(mount.path(), &["--command", "cat", "--poll-ms", "50"]);
    assert_eq!(final_status(&mine), "done");
    assert!(!other.join("status").exists());
}

// contract: A1
#[test]
fn finishes_running_sessions_on_sigterm() {
    let mount = TempDir::new().unwrap();
    let dir = session(mount.path(), HOST, "s1", "");
    let mut runner = Runner::start(
        mount.path(),
        &["--command", "sleep 1; echo finished", "--poll-ms", "50"],
    );
    wait_until("the session to start", || {
        read(&dir, "status").as_deref() == Some("running")
    });

    let pid = runner.0.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let late = session(mount.path(), HOST, "late", "");
    assert!(runner.0.wait().unwrap().success());

    assert_eq!(read(&dir, "status").unwrap(), "done");
    assert_eq!(read(&dir, "output").unwrap(), "finished");
    assert!(!late.join("status").exists());
}

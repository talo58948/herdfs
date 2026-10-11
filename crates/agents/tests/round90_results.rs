//! Result integrity before, during and after a session command.
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
    fn start(mount: &Path, command: &str, runtime: Option<&Path>) -> Self {
        let mut runner = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"));
        runner
            .arg("--mount")
            .arg(mount)
            .args(["--host", HOST, "--command", command, "--poll-ms", "20"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(runtime) = runtime {
            runner.env("XDG_RUNTIME_DIR", runtime);
        }
        Self(runner.spawn().unwrap())
    }

    fn crash(mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
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
    wait("cwd failure", || {
        fs::read_to_string(dir.join("status"))
            .is_ok_and(|status| status.trim() == "failed: cwd outside mount")
    });
}

fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
}

fn final_status(dir: &Path) -> String {
    let mut final_status = None;
    wait("terminal status", || {
        final_status = fs::read_to_string(dir.join("status"))
            .ok()
            .map(|status| status.trim().to_owned())
            .filter(|status| !matches!(status.as_str(), "queued" | "running"));
        final_status.is_some()
    });
    final_status.unwrap()
}

fn command_session(mount: &Path, name: &str) -> PathBuf {
    let dir = mount.join(".agents").join(HOST).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), "hello\n").unwrap();
    dir
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
    let _runner = Runner::start(mount.path(), "cat", None);
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

/// Result names are caller-writable, so an ordinary command must replace a planted exit code.
// contract: A2
#[test]
fn normal_runs_replace_caller_supplied_exit_codes() {
    let mount = tempfile::tempdir().unwrap();
    let dir = command_session(mount.path(), "normal");
    fs::write(dir.join("exit_code"), "caller supplied\n").unwrap();
    let _runner = Runner::start(mount.path(), "cat", None);

    assert_eq!(final_status(&dir), "done");
    assert_eq!(fs::read(dir.join("exit_code")).unwrap(), b"0\n");
}

/// A failure after spawn must preserve captured results and fill an exit code the command did not
/// reach; a normal nonzero command is covered by the ordinary session tests.
// contract: A2
#[test]
fn post_spawn_failures_keep_results_and_write_a_status() {
    let mount = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let dir = command_session(mount.path(), "post-spawn");
    let command = "echo stdout; echo stderr >&2; \
        while [ ! -s \"$HERDFS_SESSION_DIR/stdout\" ] || \
              [ ! -s \"$HERDFS_SESSION_DIR/stderr\" ]; do sleep 0.01; done; \
        record=$(find \"$XDG_RUNTIME_DIR/herdfs-agents\" -type f -print -quit); \
        rm \"$record\"; mkdir \"$record\"";
    let _runner = Runner::start(mount.path(), command, Some(runtime.path()));

    let status = final_status(&dir);
    assert!(status.starts_with("failed: "), "{status}");
    assert_eq!(
        fs::read_to_string(dir.join("exit_code")).unwrap(),
        format!("{status}\n")
    );
    assert_eq!(fs::read(dir.join("stdout")).unwrap(), b"stdout\n");
    assert_eq!(fs::read(dir.join("stderr")).unwrap(), b"stderr\n");
    let output = fs::read_to_string(dir.join("output")).unwrap();
    assert!(output.lines().any(|line| line == "stdout"));
    assert!(output.lines().any(|line| line == "stderr"));
}

/// Recovery fills the interrupted command's empty exit code without replacing its captures.
// contract: A2
#[test]
fn recovery_keeps_results_and_writes_the_restart_status() {
    let mount = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let dir = command_session(mount.path(), "recovery");
    let command = "echo saved-out; echo saved-err >&2; \
        while [ ! -s \"$HERDFS_SESSION_DIR/stdout\" ] || \
              [ ! -s \"$HERDFS_SESSION_DIR/stderr\" ]; do sleep 0.01; done; \
        touch \"$HERDFS_SESSION_DIR/ready\"; exec sleep 30";
    let runner = Runner::start(mount.path(), command, Some(runtime.path()));
    wait("running command", || dir.join("ready").exists());
    runner.crash();

    let _replacement = Runner::start(mount.path(), "exit 0", Some(runtime.path()));
    assert_eq!(final_status(&dir), "failed: runner restarted");
    assert_eq!(
        fs::read(dir.join("exit_code")).unwrap(),
        b"failed: runner restarted\n"
    );
    assert_eq!(fs::read(dir.join("stdout")).unwrap(), b"saved-out\n");
    assert_eq!(fs::read(dir.join("stderr")).unwrap(), b"saved-err\n");
    let output = fs::read_to_string(dir.join("output")).unwrap();
    assert!(output.lines().any(|line| line == "saved-out"));
    assert!(output.lines().any(|line| line == "saved-err"));
}

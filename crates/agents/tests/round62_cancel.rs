//! Round 62: detached children must not delay the tracked session's cancellation status.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct Runner(Child);

impl Runner {
    fn start(mount: &Path, redirect_detached: bool) -> Self {
        let redirect = redirect_detached
            .then_some("</dev/null >/dev/null 2>&1")
            .unwrap_or("");
        let command =
            format!("setsid sleep 30 {redirect} & echo $! > \"$HERDFS_SESSION_DIR/detached.pid\"; wait");
        let child = Command::new(env!("CARGO_BIN_EXE_herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .args([
                "--host",
                "box",
                "--command",
                &command,
                "--poll-ms",
                "20",
                "--max-sessions",
                "4",
            ])
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
    fs::write(dir.join("prompt"), "run\n").unwrap();
    dir
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn wait_text(path: &Path, wanted: Option<&str>) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(text) = read(path).filter(|text| wanted.is_none_or(|wanted| text == wanted)) {
            return text;
        }
        assert!(Instant::now() < deadline, "{} was not ready", path.display());
        sleep(Duration::from_millis(20));
    }
}

fn kill(pid: &str) {
    let _ = Command::new("kill")
        .args(["-KILL", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A detached child with redirected pipes controls three detached children that keep their
/// inherited capture pipes. The untracked child may survive, but it must not leave the tracked
/// session `running` beyond A2's five-second cancellation bound.
// contract: A2
#[test]
fn detached_capture_pipes_do_not_delay_cancellation() {
    let control_mount = TempDir::new().unwrap();
    let control = session(control_mount.path(), "control");
    let _control_runner = Runner::start(control_mount.path(), true);
    wait_text(&control.join("status"), Some("running"));
    let control_pid = wait_text(&control.join("detached.pid"), None);
    let started = Instant::now();
    fs::write(control.join("cancel"), "").unwrap();
    wait_text(&control.join("status"), Some("cancelled"));
    assert!(started.elapsed() < Duration::from_secs(5));
    kill(&control_pid);

    let mount = TempDir::new().unwrap();
    let attacks: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("attack-{trial}")))
        .collect();
    let _runner = Runner::start(mount.path(), false);
    let mut pids = Vec::new();
    for dir in &attacks {
        wait_text(&dir.join("status"), Some("running"));
        pids.push(wait_text(&dir.join("detached.pid"), None));
        fs::write(dir.join("cancel"), "").unwrap();
    }
    sleep(Duration::from_millis(5_500));
    let statuses: Vec<_> = attacks.iter().map(|dir| read(&dir.join("status"))).collect();
    for pid in &pids {
        kill(pid);
    }
    for dir in &attacks {
        wait_text(&dir.join("status"), Some("cancelled"));
    }
    let delayed = statuses
        .iter()
        .filter(|status| status.as_deref() != Some("cancelled"))
        .count();
    eprintln!("statuses after 5.5 s: {statuses:?}");
    assert_eq!(delayed, 0, "{delayed}/3 sessions exceeded A2");
}

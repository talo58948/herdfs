//! Round 101: recovery of running sessions moved below another host child.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
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
            .args([
                "--host",
                "box",
                "--command",
                "echo $$ > \"$HERDFS_SESSION_DIR/pid\"; exec sleep 30",
                "--max-sessions",
                "6",
                "--poll-ms",
                "20",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
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

fn session(mount: &Path, name: &str) -> PathBuf {
    let dir = mount.join(".agents/box").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt"), "run\n").unwrap();
    dir
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|text| text.trim().to_owned())
}

fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
}

fn pid(dir: &Path) -> String {
    wait("running session", || {
        read(&dir.join("status")).as_deref() == Some("running") && dir.join("pid").exists()
    });
    read(&dir.join("pid")).unwrap()
}

fn process_exists(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn kill_group(pid: &str) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .status();
}

/// Recovery must find a held running session after any rename within its writable host tree;
/// renames that keep sessions as direct host children are the passing controls.
// contract: A1, A2
#[test]
#[ignore = "REJECTED ROUND101: sessions moved out of the host directory by a principal who may spawn them are untracked, like setsid (A2); see docs/architecture.md"]
fn nested_renamed_sessions_remain_contained_across_restart() {
    let mount = TempDir::new().unwrap();
    let attacks: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("attack-{trial}")))
        .collect();
    let controls: Vec<_> = (0..3)
        .map(|trial| session(mount.path(), &format!("control-{trial}")))
        .collect();
    let runner = Runner::start(mount.path());
    let attack_pids: Vec<_> = attacks.iter().map(|dir| pid(dir)).collect();
    let control_pids: Vec<_> = controls.iter().map(|dir| pid(dir)).collect();

    let container = mount.path().join(".agents/box/container");
    fs::create_dir(&container).unwrap();
    let nested: Vec<_> = attacks
        .iter()
        .map(|dir| {
            let destination = container.join(dir.file_name().unwrap());
            fs::rename(dir, &destination).unwrap();
            destination
        })
        .collect();
    let renamed_controls: Vec<_> = controls
        .iter()
        .enumerate()
        .map(|(trial, dir)| {
            let destination = mount.path().join(format!(".agents/box/renamed-control-{trial}"));
            fs::rename(dir, &destination).unwrap();
            destination
        })
        .collect();
    runner.crash();

    let _replacement = Runner::start(mount.path());
    wait("control recovery", || {
        renamed_controls
            .iter()
            .all(|dir| read(&dir.join("status")).as_deref() == Some("failed: runner restarted"))
    });
    wait("control process groups to stop", || {
        control_pids.iter().all(|pid| !process_exists(pid))
    });
    let returned: Vec<_> = nested
        .iter()
        .enumerate()
        .map(|(trial, dir)| {
            let destination = mount.path().join(format!(".agents/box/returned-attack-{trial}"));
            fs::rename(dir, &destination).unwrap();
            destination
        })
        .collect();
    sleep(Duration::from_millis(500));

    let survived = attack_pids.iter().filter(|pid| process_exists(pid)).count();
    let still_running = returned
        .iter()
        .filter(|dir| read(&dir.join("status")).as_deref() == Some("running"))
        .count();
    for pid in &attack_pids {
        kill_group(pid);
    }
    assert_eq!(
        (survived, still_running),
        (0, 0),
        "{survived}/3 nested process groups survived and {still_running}/3 statuses remained running"
    );
}

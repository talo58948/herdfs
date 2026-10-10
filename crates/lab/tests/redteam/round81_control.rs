//! Round 81: an old mount must not erase its replacement's control registration.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy, herdfs_command};
use serde::Deserialize;

#[derive(Deserialize)]
struct Registration {
    mountpoint: PathBuf,
    pid: u32,
}

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn registrations() -> Result<Vec<Registration>> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(config) => PathBuf::from(config),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    let directory = config.join("herdfs/mounts");
    let mut registrations = Vec::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        if let Ok(registration) = serde_json::from_slice(&bytes) {
            registrations.push(registration);
        }
    }
    Ok(registrations)
}

fn wait_registered(mountpoint: &Path, pid: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if registrations()?
            .iter()
            .any(|registration| registration.mountpoint == mountpoint && registration.pid == pid)
        {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "replacement mount did not register");
        thread::sleep(Duration::from_millis(50));
    }
}

fn status(path: &Path) -> Result<std::process::Output> {
    Ok(herdfs_command()?
        .args(["--json", "status"])
        .arg(path)
        .output()?)
}

fn replacement(lab: &Lab, root: &Path) -> Result<Process> {
    let cache = root.join("cache");
    let journal = root.join("journal");
    fs::create_dir_all(&cache)?;
    fs::create_dir_all(&journal)?;
    let child = herdfs_command()?
        .args(["mount", "--authority", &lab.authority_url("a")])
        .args(["--space", herdfs_lab::SPACE, "--token-file"])
        .arg(lab.client_dir("a").join("token"))
        .arg("--cache")
        .arg(cache)
        .args(["--cache-bytes", "1048576", "--journal"])
        .arg(journal)
        .args(["--strategy", "stream", "--nfs"])
        .arg(lab.mount_path("a"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(Process(child))
}

/// A live status controls three replacement registrations. Once each replacement is registered,
/// the superseded mount's graceful exit must not make the still-running replacement disappear.
// contract: U1
#[test]
#[ignore = "ROUND81: an exiting old mount deletes its live replacement's registration"]
fn round81_old_mount_exit_does_not_unregister_its_replacement() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let mut failures = 0;
    for trial in 0..3 {
        let mut lab =
            Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
        let control = status(lab.mount_path("a"))?;
        ensure!(control.status.success(), "trial {trial} live control failed");

        let scratch = tempfile::tempdir()?;
        let replacement = replacement(&lab, scratch.path())?;
        wait_registered(lab.mount_path("a"), replacement.0.id())?;
        let old = lab.client_pid("a").context("old mount has no pid")?;
        let stopped = Command::new("kill").args(["-TERM", &old.to_string()]).status()?;
        ensure!(stopped.success(), "stopping old mount failed");

        let deadline = Instant::now() + Duration::from_secs(10);
        let after = loop {
            let after = status(lab.mount_path("a"))?;
            if !after.status.success() || Instant::now() >= deadline {
                break after;
            }
            thread::sleep(Duration::from_millis(50));
        };
        eprintln!(
            "trial {trial}: replacement status succeeded={}",
            after.status.success()
        );
        failures += usize::from(!after.status.success());
        drop(replacement);
        lab.kill_client("a")?;
    }
    assert_eq!(
        failures, 0,
        "replacement registration lost in {failures}/3 trials"
    );
    Ok(())
}

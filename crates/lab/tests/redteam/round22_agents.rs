//! Red team, round 22: the agents runner's host name must stay inside the public mount.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::Result;
use tempfile::TempDir;

struct Runner(Child);

impl Runner {
    fn start(mount: &Path, host: &Path) -> Result<Self> {
        let target = env::var_os("CARGO_TARGET_DIR").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"),
            PathBuf::from,
        );
        let child = Command::new(target.join("debug/herdfs-agents"))
            .arg("--mount")
            .arg(mount)
            .arg("--host")
            .arg(host)
            .args(["--command", "cat; echo ran", "--poll-ms", "20"])
            .spawn()?;
        Ok(Self(child))
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn session(root: &Path, host: &Path) -> Result<PathBuf> {
    let dir = root.join(host).join("s1");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("prompt"), "hello\n")?;
    Ok(dir)
}

fn wait_for(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        sleep(Duration::from_millis(20));
    }
    false
}

/// A one-component host reads its sessions from `/.agents/<host>/` in the public mount.
// contract: A1
#[test]
fn a_component_host_runs_only_its_public_mount_sessions() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let mount = TempDir::new()?;
    let host = Path::new("box");
    let dir = session(&mount.path().join(".agents"), host)?;
    let _runner = Runner::start(mount.path(), host)?;

    assert!(wait_for(&dir.join("status")));
    assert!(wait_for(&dir.join("output")));
    Ok(())
}

/// Repro: an absolute `--host` replaces `<mount>/.agents` during `PathBuf::join`. The runner then
/// claims and executes a session in an ordinary host directory, although A1 permits spawning only
/// through the corresponding directory in the public filesystem.
// contract: A1
#[test]
#[ignore = "ROUND22: an absolute agents host executes sessions outside the public mount"]
fn an_agents_host_cannot_escape_the_public_mount() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let mount = TempDir::new()?;
    let outside = TempDir::new()?;
    let host = outside.path().join("host");
    let dir = session(Path::new("/"), &host)?;
    let _runner = Runner::start(mount.path(), &host)?;

    sleep(Duration::from_secs(1));
    assert!(
        !dir.join("status").exists(),
        "runner claimed a session outside the mount"
    );
    assert!(
        !dir.join("output").exists(),
        "runner ran a session outside the mount"
    );
    Ok(())
}

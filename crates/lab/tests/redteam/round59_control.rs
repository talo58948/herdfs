//! Round 59: paused uploads use the offline journal boundary until they resume.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, ensure};
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, Strategy, herdfs_command};

use crate::{VISIBLE_AFTER, save};

fn command(lab: &Lab, op: &str) -> Result<()> {
    let output = herdfs_command()?
        .args(["--json", op])
        .arg(lab.mount_path("a"))
        .output()?;
    ensure!(
        output.status.success(),
        "{op} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn rewrite(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// An unpaused fsync controls three paused fsyncs. The control reaches the authority before it
/// returns; paused fsyncs return at the journal boundary and reach the authority after resume.
// contract: C2, C4a, C12, U1
#[test]
fn round59_paused_fsync_journals_until_resume() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() == Ok("nfs") {
        eprintln!("skipped: the default-mount probe runs separately from NFS");
        return Ok(());
    }
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )?;
    save(&lab.mount_path("a").join("fsync-control"), b"before")?;
    lab.wait_committed("a")?;
    let control = lab.head(ADMIN, "fsync-control")?.id;
    rewrite(&lab.mount_path("a").join("fsync-control"), b"after")?;
    ensure!(
        lab.head(ADMIN, "fsync-control")?.id != control,
        "unpaused fsync returned before the authority commit"
    );
    ensure!(lab.stats("a")?["pending_commits"] == 0);
    thread::sleep(VISIBLE_AFTER);
    ensure!(fs::read(lab.mount_path("b").join("fsync-control"))? == b"after");

    let mut before = Vec::new();
    for trial in 0..3 {
        let name = format!("paused-fsync-{trial}");
        save(&lab.mount_path("a").join(&name), b"before")?;
        let version = lab.head(ADMIN, &name)?.id;
        before.push((name, version));
    }
    lab.wait_committed("a")?;
    command(&lab, "pause")?;

    for (name, version) in &before {
        let path = lab.mount_path("a").join(name);
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = (|| -> Result<()> {
                let mut file = File::create(path)?;
                file.write_all(b"after")?;
                started_tx.send(())?;
                file.sync_all()?;
                Ok(())
            })();
            let _ = done_tx.send(result);
        });
        started_rx.recv_timeout(Duration::from_secs(10))?;
        match done_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(result) => result?,
            Err(error) => {
                command(&lab, "resume")?;
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("fsync worker panicked"))?;
                return Err(error.into());
            }
        }
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("fsync worker panicked"))?;
        ensure!(
            lab.head(ADMIN, name)?.id == *version,
            "paused fsync committed before resume"
        );
    }
    ensure!(lab.stats("a")?["pending_commits"] == 3);

    command(&lab, "resume")?;
    lab.wait_committed("a")?;
    for (name, version) in &before {
        ensure!(
            lab.head(ADMIN, name)?.id != *version,
            "{name} did not commit after resume"
        );
    }
    thread::sleep(VISIBLE_AFTER);
    for (name, _) in before {
        ensure!(fs::read(lab.mount_path("b").join(name))? == b"after");
    }
    Ok(())
}

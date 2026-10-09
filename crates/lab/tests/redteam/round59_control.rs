//! Round 59: pausing uploads must not turn fsync into a journal-only success.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy, herdfs_command};

use crate::{VISIBLE_AFTER, save};

fn command(lab: &Lab, op: &str) -> Result<()> {
    let output = herdfs_command()?
        .args(["--json", op])
        .arg(lab.mount_path("a"))
        .output()?;
    ensure!(output.status.success(), "{op} failed: {}", String::from_utf8_lossy(&output.stderr));
    Ok(())
}

fn rewrite(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// An unpaused fsync controls three paused fsyncs. While the authority is reachable, fsync must
/// remain an authority commit barrier even when the user has paused background uploads.
// contract: C2, C4a, U1
#[test]
fn round59_paused_fsync_does_not_report_an_uncommitted_write() -> Result<()> {
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
    rewrite(&lab.mount_path("a").join("fsync-control"), b"after")?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    ensure!(fs::read(lab.mount_path("b").join("fsync-control"))? == b"after");

    for trial in 0..3 {
        save(&lab.mount_path("a").join(format!("paused-fsync-{trial}")), b"before")?;
    }
    lab.wait_committed("a")?;
    command(&lab, "pause")?;

    let mut completions = Vec::new();
    for trial in 0..3 {
        let path = lab.mount_path("a").join(format!("paused-fsync-{trial}"));
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
        completions.push((done_rx, worker));
    }

    let mut early = 0;
    for (trial, (done, _)) in completions.iter().enumerate() {
        match done.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => {
                result?;
                eprintln!("trial {trial}: paused fsync returned before resume");
                early += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => return Err(error.into()),
        }
    }
    command(&lab, "resume")?;
    for (done, worker) in completions {
        if early < 3 {
            match done.recv_timeout(Duration::from_secs(30)) {
                Ok(result) => result?,
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
                Err(error) => return Err(error.into()),
            }
        }
        worker.join().map_err(|_| anyhow::anyhow!("fsync worker panicked"))?;
    }
    lab.wait_committed("a")?;
    assert_eq!(early, 0, "paused fsyncs that returned before authority commit, of 3");
    Ok(())
}

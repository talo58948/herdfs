//! Round 23: settling a closed write before renaming it through moved parent directories.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use herdfs_core::chunk;

use crate::{VISIBLE_AFTER, random_bytes, save, two_clients};

const NEW_LEN: usize = 8 << 20;
const TRIALS: usize = 3;
const STALL: Duration = Duration::from_secs(1);
const FINISH_WITHIN: Duration = Duration::from_secs(30);

/// The pid of the lab's MinIO, whose command line names its data directory.
fn store_pid(store_dir: &Path) -> Result<i32> {
    let wanted = store_dir.as_os_str().as_encoded_bytes();
    for proc in fs::read_dir("/proc")? {
        let proc = proc?;
        let Some(pid) = proc.file_name().to_str().and_then(|pid| pid.parse().ok()) else {
            continue;
        };
        let Ok(cmdline) = fs::read(proc.path().join("cmdline")) else {
            continue;
        };
        if cmdline.split(|&byte| byte == 0).any(|arg| arg == wanted) {
            return Ok(pid);
        }
    }
    bail!("no process serves {}", store_dir.display())
}

/// Keeps MinIO stopped until dropped.
struct Stopped(i32);

impl Stopped {
    fn new(pid: i32) -> Result<Self> {
        let status = Command::new("kill").args(["-STOP", &pid.to_string()]).status()?;
        ensure!(status.success(), "stop MinIO {pid}");
        Ok(Self(pid))
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = Command::new("kill").args(["-CONT", &self.0.to_string()]).status();
    }
}

/// Closes b's rewrite, optionally moves and reuses both parent paths, then renames the file
/// through the held directory descriptors while its upload is stalled. Returns whether rename
/// finished before the store came back and the head size it exposed then.
fn rename_while_stalled(lab: &herdfs_lab::Lab, trial: usize, move_parents: bool) -> Result<(bool, u64)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let source = format!("source-{move_parents}-{trial}");
    let destination = format!("destination-{move_parents}-{trial}");
    fs::create_dir(a.join(&source))?;
    fs::create_dir(a.join(&destination))?;
    save(&a.join(&source).join("file"), b"old")?;
    thread::sleep(VISIBLE_AFTER);

    let source_fd = File::open(b.join(&source))?;
    let destination_fd = File::open(b.join(&destination))?;
    let source_file = PathBuf::from(format!("/proc/self/fd/{}/file", source_fd.as_raw_fd()));
    let destination_file = PathBuf::from(format!("/proc/self/fd/{}/file", destination_fd.as_raw_fd()));
    fs::read(&source_file).context("warm source file")?;
    thread::sleep(VISIBLE_AFTER);
    let stopped = Stopped::new(store_pid(lab.store_dir())?)?;
    fs::write(&source_file, random_bytes(NEW_LEN)).context("close rewrite")?;

    let (source_now, destination_now) = if move_parents {
        let source_now = format!("{source}-moved");
        let destination_now = format!("{destination}-moved");
        fs::rename(a.join(&source), a.join(&source_now))?;
        fs::rename(a.join(&destination), a.join(&destination_now))?;
        fs::create_dir(a.join(&source))?;
        fs::create_dir(a.join(&destination))?;
        (source_now, destination_now)
    } else {
        (source, destination)
    };
    let (sent, finished) = mpsc::channel();
    thread::spawn(move || sent.send(fs::rename(source_file, destination_file)));
    let early = finished.recv_timeout(STALL).ok();
    let finished_early = early.is_some();
    if let Some(Err(err)) = &early {
        bail!("rename through held parents: {err}");
    }
    let size = if finished_early {
        let head = lab.head(herdfs_lab::ADMIN, &format!("{destination_now}/file"))?;
        chunk::total_len(&head.chunks)
    } else {
        NEW_LEN as u64
    };
    drop(stopped);
    let renamed = match early {
        Some(renamed) => renamed,
        None => finished
            .recv_timeout(FINISH_WITHIN)
            .map_err(|_| anyhow::anyhow!("rename hung after the store recovered"))?,
    };
    renamed.context("rename through held parents")?;
    lab.wait_committed("b")?;
    if a.join(source_now).join("file").exists() {
        bail!("source still holds file after rename");
    }
    Ok((finished_early, size))
}

/// A rename must wait for the source's closed write before exposing it at the destination. The
/// control leaves both parent paths fixed; three attacks move and reuse both paths first.
// contract: C1, C3, C7
#[test]
fn a_rename_through_moved_parents_settles_its_closed_write_first() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    assert_eq!(
        rename_while_stalled(&lab, 0, false)?,
        (false, NEW_LEN as u64),
        "control"
    );
    let outcomes = (0..TRIALS)
        .map(|trial| rename_while_stalled(&lab, trial, true))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        outcomes,
        vec![(false, NEW_LEN as u64); TRIALS],
        "whether rename returned before commit and the heads it exposed"
    );
    Ok(())
}

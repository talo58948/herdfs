//! Round 27: operations through held directories while they move or relocate.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use herdfs_core::chunk;
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, random_bytes, save, two_clients};

const SLOW_AUTHORITY: Profile = Profile::Custom {
    round_trip_ms: 2_000,
    kilobytes_per_sec: 100_000,
    store_first_byte_ms: 0,
};
const MOVE_EVERY: Duration = Duration::from_millis(200);
const INSTALL_AFTER: Duration = Duration::from_millis(3_500);
const TRIALS: usize = 3;
const NEW_LEN: usize = 8 << 20;
const STALL: Duration = Duration::from_secs(1);
const FINISH_WITHIN: Duration = Duration::from_secs(30);

fn start() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], SLOW_AUTHORITY, Strategy::Stream);
    Lab::start(config).map(Some)
}

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
    Err(anyhow!("no process serves {}", store_dir.display()))
}

struct Stopped(i32);

impl Stopped {
    fn new(pid: i32) -> Result<Self> {
        ensure!(
            Command::new("kill")
                .args(["-STOP", &pid.to_string()])
                .status()?
                .success(),
            "stop MinIO {pid}"
        );
        Ok(Self(pid))
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = Command::new("kill").args(["-CONT", &self.0.to_string()]).status();
    }
}

/// Races O_CREAT with another client installing the child after b's negative lookup, while a
/// optionally keeps moving the held parent after that.
fn open_existing(lab: &Lab, trial: usize, moving: bool) -> Result<(std::io::Result<File>, PathBuf, usize)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let first = format!("create-{moving}-{trial}-0");
    fs::create_dir(a.join(&first))?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join(&first))?;
    thread::sleep(VISIBLE_AFTER);
    let child = PathBuf::from(format!("/proc/self/fd/{}/child", held.as_raw_fd()));
    let stop = AtomicBool::new(false);
    let (opened, last, moves) = thread::scope(|scope| {
        let opener = scope.spawn(|| {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(child)
        });
        let rename = scope.spawn(|| -> Result<(PathBuf, usize)> {
            let mut current = a.join(first);
            let mut moves = 0;
            thread::sleep(INSTALL_AFTER);
            save(&current.join("child"), b"kept")?;
            while moving && !stop.load(Relaxed) {
                let next = a.join(format!("create-{moving}-{trial}-{}", moves + 1));
                fs::rename(&current, &next)?;
                current = next;
                moves += 1;
                thread::sleep(MOVE_EVERY);
            }
            Ok((current, moves))
        });
        let opened = opener.join().map_err(|_| anyhow!("open thread panicked"))?;
        stop.store(true, Relaxed);
        let (last, moves) = rename.join().map_err(|_| anyhow!("rename thread panicked"))??;
        Ok::<_, anyhow::Error>((opened, last, moves))
    })?;
    Ok((opened, last, moves))
}

/// O_CREAT on an existing entry is still an operation through the held directory and follows it
/// while it moves. The control opens the same kind of entry with its parent fixed.
// contract: C7
#[test]
#[ignore = "ROUND27: O_CREAT loses a continuously renamed held parent when the child exists"]
fn open_with_create_follows_a_parent_that_keeps_moving() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let (opened, parent, moves) = open_existing(&lab, 0, false)?;
    assert!(
        opened.is_ok() && parent.join("child").is_file(),
        "control: {opened:?}"
    );
    assert_eq!(moves, 0, "control parent moves");

    let mut failures = 0;
    for trial in 0..TRIALS {
        let (opened, parent, moves) = open_existing(&lab, trial, true)?;
        ensure!(moves > 2, "trial {trial} moved only {moves} times");
        let landed = parent.join("child").is_file();
        eprintln!("trial {trial}: {moves} moves, open={opened:?}, landed={landed}");
        failures += usize::from(opened.is_err() || !landed);
    }
    assert_eq!(
        failures, 0,
        "O_CREAT opens that missed the held parent, of {TRIALS}"
    );
    Ok(())
}

/// Removes a child through b's held directory while a optionally keeps renaming that directory.
fn remove_child(lab: &Lab, trial: usize, moving: bool) -> Result<(std::io::Result<()>, PathBuf, usize)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let first = format!("remove-{moving}-{trial}-0");
    fs::create_dir(a.join(&first))?;
    save(&a.join(&first).join("child"), b"remove me")?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join(&first))?;
    thread::sleep(VISIBLE_AFTER);
    let child = PathBuf::from(format!("/proc/self/fd/{}/child", held.as_raw_fd()));
    let stop = AtomicBool::new(false);
    let (removed, last, moves) = thread::scope(|scope| {
        let remover = scope.spawn(|| fs::remove_file(child));
        let rename = scope.spawn(|| -> Result<(PathBuf, usize)> {
            let mut current = a.join(first);
            let mut moves = 0;
            while moving && !stop.load(Relaxed) {
                let next = a.join(format!("remove-{moving}-{trial}-{}", moves + 1));
                fs::rename(&current, &next)?;
                current = next;
                moves += 1;
                thread::sleep(MOVE_EVERY);
            }
            Ok((current, moves))
        });
        let removed = remover.join().map_err(|_| anyhow!("remove thread panicked"))?;
        stop.store(true, Relaxed);
        let (last, moves) = rename.join().map_err(|_| anyhow!("rename thread panicked"))??;
        Ok::<_, anyhow::Error>((removed, last, moves))
    })?;
    Ok((removed, last, moves))
}

/// Unlink through a held directory reaches the named child while that directory keeps moving.
/// The control removes a child with its parent fixed.
// contract: C7
#[test]
#[ignore = "ROUND27: lookup loses a continuously renamed held parent before unlink"]
fn unlink_follows_a_parent_that_keeps_moving() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let (removed, parent, moves) = remove_child(&lab, 0, false)?;
    assert!(
        removed.is_ok() && !parent.join("child").exists(),
        "control: {removed:?}"
    );
    assert_eq!(moves, 0, "control parent moves");

    let mut failures = 0;
    for trial in 0..TRIALS {
        let (removed, parent, moves) = remove_child(&lab, trial, true)?;
        ensure!(moves > 2, "trial {trial} moved only {moves} times");
        let remains = parent.join("child").exists();
        eprintln!("trial {trial}: {moves} moves, unlink={removed:?}, remains={remains}");
        failures += usize::from(removed.is_err() || remains);
    }
    assert_eq!(failures, 0, "unlinks that missed the held parent, of {TRIALS}");
    Ok(())
}

/// Rewrites a child, moves both held parents, lets b relocate only the parents, then renames the
/// child while its upload cannot finish. Returns whether rename exposed the old head early.
fn rename_after_parents_move(lab: &Lab, trial: usize, move_parents: bool) -> Result<(bool, u64)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let source = format!("settle-source-{move_parents}-{trial}");
    let destination = format!("settle-destination-{move_parents}-{trial}");
    fs::create_dir(a.join(&source))?;
    fs::create_dir(a.join(&destination))?;
    save(&a.join(&source).join("child"), b"old")?;
    thread::sleep(VISIBLE_AFTER);

    let source_fd = File::open(b.join(&source))?;
    let destination_fd = File::open(b.join(&destination))?;
    let source_child = PathBuf::from(format!("/proc/self/fd/{}/child", source_fd.as_raw_fd()));
    let destination_child = PathBuf::from(format!("/proc/self/fd/{}/child", destination_fd.as_raw_fd()));
    fs::read(&source_child)?;
    let stopped = Stopped::new(store_pid(lab.store_dir())?)?;
    fs::write(&source_child, random_bytes(NEW_LEN))?;
    ensure!(lab.stats("b")?["pending_commits"] == 1, "rewrite is pending");

    let (source_now, destination_now) = if move_parents {
        let source_now = format!("{source}-moved");
        let destination_now = format!("{destination}-moved");
        fs::rename(a.join(&source), a.join(&source_now))?;
        fs::rename(a.join(&destination), a.join(&destination_now))?;
        fs::create_dir(a.join(&source))?;
        fs::create_dir(a.join(&destination))?;
        thread::sleep(VISIBLE_AFTER);
        source_fd.metadata()?;
        destination_fd.metadata()?;
        (source_now, destination_now)
    } else {
        (source, destination)
    };

    let (sent, finished) = mpsc::channel();
    thread::spawn(move || sent.send(fs::rename(source_child, destination_child)));
    let early = finished.recv_timeout(STALL).ok();
    if let Some(Err(err)) = &early {
        return Err(anyhow!("rename through held parents: {err}"));
    }
    let size = match early {
        Some(Ok(())) => {
            let head = lab.head(ADMIN, &format!("{destination_now}/child"))?;
            chunk::total_len(&head.chunks)
        }
        _ => NEW_LEN as u64,
    };
    drop(stopped);
    if early.is_none() {
        finished
            .recv_timeout(FINISH_WITHIN)
            .map_err(|_| anyhow!("rename hung after the store recovered"))??;
    }
    lab.wait_committed("b")?;
    ensure!(
        !a.join(source_now).join("child").exists(),
        "source still has child"
    );
    Ok((early.is_some(), size))
}

/// A rename waits for the closed write of its source after held parent inodes have relocated.
/// The control keeps both parents in place; attacks move and reuse both old parent paths.
// contract: C1, C3, C7
#[test]
fn rename_settles_a_child_after_its_held_parents_relocate() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    assert_eq!(
        rename_after_parents_move(&lab, 0, false)?,
        (false, NEW_LEN as u64),
        "control"
    );
    let outcomes = (0..TRIALS)
        .map(|trial| rename_after_parents_move(&lab, trial, true))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        outcomes,
        vec![(false, NEW_LEN as u64); TRIALS],
        "whether rename returned before commit and the heads it exposed"
    );
    Ok(())
}

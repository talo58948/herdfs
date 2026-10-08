//! Round 35: id-addressed operations through entries held while another client moves them.

#![cfg_attr(
    target_os = "linux",
    expect(unsafe_code, reason = "the held-symlink probe requires Linux O_PATH")
)]

#[cfg(target_os = "linux")]
use std::ffi::{CString, OsString};
use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::thread;

use anyhow::{Result, ensure};
use herdfs_lab::Links;

use crate::{
    VISIBLE_AFTER, assert_bytes, entry_names, mkdir_entry, readlink_entry, rename_entry,
    rmdir_entry, save, symlink_entry, two_clients, unlink_entry, write_entry,
};

const TRIALS: usize = 3;
type ComparedBytes = Option<(Vec<u8>, Option<Vec<u8>>)>;

/// Exercises child operations through a directory descriptor after its path is moved and reused.
fn directory_ops(trial: usize, moved: bool) -> Result<Option<()>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let old = format!("held-{moved}-{trial}");
    let relocated = format!("relocated-{moved}-{trial}");
    let original = a.join(&old);
    fs::create_dir(&original)?;
    save(&original.join("victim"), b"original victim")?;
    save(&original.join("source"), b"original source")?;
    save(&original.join("listed-original"), b"original")?;
    fs::create_dir(original.join("empty"))?;
    thread::sleep(VISIBLE_AFTER);

    let held = File::open(b.join(&old))?;
    let mut before = entry_names(&held)?;
    before.sort();
    ensure!(before.len() == 4, "control lists all original children");

    let authoritative = if moved {
        fs::rename(&original, a.join(&relocated))?;
        fs::create_dir(&original)?;
        save(&original.join("victim"), b"replacement victim")?;
        save(&original.join("source"), b"replacement source")?;
        save(&original.join("listed-replacement"), b"replacement")?;
        fs::create_dir(original.join("empty"))?;
        a.join(&relocated)
    } else {
        original.clone()
    };
    thread::sleep(VISIBLE_AFTER);
    let listed = entry_names(&held)?;
    ensure!(
        listed.iter().any(|name| name == "listed-original"),
        "readdir follows held parent"
    );
    ensure!(!listed.iter().any(|name| name == "listed-replacement"));

    write_entry(&held, "created", b"created")?;
    mkdir_entry(&held, "made-dir")?;
    symlink_entry(&held, "made-link", "created")?;
    unlink_entry(&held, "victim")?;
    rmdir_entry(&held, "empty")?;
    rename_entry(&held, "source", &held, "destination")?;

    ensure!(
        authoritative.join("created").is_file(),
        "create follows held parent"
    );
    ensure!(
        authoritative.join("made-dir").is_dir(),
        "mkdir follows held parent"
    );
    ensure!(readlink_entry(&held, "made-link")? == PathBuf::from("created").into_os_string());
    ensure!(
        !authoritative.join("victim").exists(),
        "unlink follows held parent"
    );
    ensure!(!authoritative.join("empty").exists(), "rmdir follows held parent");
    assert_bytes(
        &fs::read(authoritative.join("destination"))?,
        b"original source",
        "renamed original child",
    );
    if moved {
        assert_bytes(
            &fs::read(original.join("victim"))?,
            b"replacement victim",
            "replacement victim",
        );
        assert_bytes(
            &fs::read(original.join("source"))?,
            b"replacement source",
            "replacement source",
        );
        ensure!(original.join("empty").is_dir(), "replacement directory remains");
        ensure!(
            !original.join("created").exists(),
            "create did not use the old path"
        );
    }
    Ok(Some(()))
}

/// Every child operation through a held directory follows its entry after another client moves
/// it and recreates its old path. A fixed directory is the control; three moved trials attack it.
// contract: C7
#[test]
fn child_operations_follow_a_held_recreated_directory() -> Result<()> {
    if directory_ops(0, false)?.is_none() {
        return Ok(());
    }
    for trial in 0..TRIALS {
        directory_ops(trial, true)?;
    }
    Ok(())
}

/// Rewrites through b's descriptor after optionally moving the file and reusing its old name.
fn rewrite_held(trial: usize, moved: bool) -> Result<ComparedBytes> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let old = format!("file-{moved}-{trial}");
    let relocated = format!("file-moved-{moved}-{trial}");
    save(&a.join(&old), b"original bytes")?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::options().read(true).write(true).open(b.join(&old))?;

    let authoritative = if moved {
        fs::rename(a.join(&old), a.join(&relocated))?;
        save(&a.join(&old), b"replacement bytes")?;
        a.join(&relocated)
    } else {
        a.join(&old)
    };
    thread::sleep(VISIBLE_AFTER);
    held.set_len(0)?;
    held.write_all_at(b"held rewrite", 0)?;
    held.sync_all()?;
    drop(held);
    thread::sleep(VISIBLE_AFTER);
    let replacement = moved.then(|| fs::read(a.join(&old))).transpose()?;
    Ok(Some((fs::read(authoritative)?, replacement)))
}

/// Setattr and writes through a held file descriptor reach that entry after its old name is
/// recreated. A stationary file is the control; three rename-and-recreate trials attack it.
// contract: C1, C7
#[test]
fn truncate_and_write_follow_a_held_recreated_file() -> Result<()> {
    let Some((control, None)) = rewrite_held(0, false)? else {
        return Ok(());
    };
    assert_bytes(&control, b"held rewrite", "control rewrite");
    for trial in 0..TRIALS {
        let Some((rewritten, replacement)) = rewrite_held(trial, true)? else {
            return Ok(());
        };
        assert_bytes(&rewritten, b"held rewrite", "held entry rewrite");
        assert_bytes(
            &replacement.expect("moved trial has a replacement"),
            b"replacement bytes",
            "replacement bytes",
        );
    }
    Ok(())
}

/// Recovers an offline rewrite after a move, old-name reuse, and client crash.
fn recover_held(trial: usize, moved: bool) -> Result<ComparedBytes> {
    let Some(mut lab) = two_clients()? else {
        return Ok(None);
    };
    let a = lab.mount_path("a").to_owned();
    let b = lab.mount_path("b").to_owned();
    let old = format!("crash-{moved}-{trial}");
    let relocated = format!("crash-moved-{moved}-{trial}");
    save(&a.join(&old), b"original bytes")?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::options().read(true).write(true).open(b.join(&old))?;

    let authoritative = if moved {
        fs::rename(a.join(&old), a.join(&relocated))?;
        save(&a.join(&old), b"replacement bytes")?;
        a.join(&relocated)
    } else {
        a.join(&old)
    };
    lab.cut("b", Links::Authority)?;
    held.set_len(0)?;
    held.write_all_at(b"recovered rewrite", 0)?;
    drop(held);
    ensure!(lab.stats("b")?["pending_commits"] == 1, "rewrite is journaled");
    lab.kill_client("b")?;
    lab.restore("b", Links::Authority)?;
    lab.restart_client("b")?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    let replacement = moved.then(|| fs::read(a.join(&old))).transpose()?;
    Ok(Some((fs::read(authoritative)?, replacement)))
}

/// A closed offline write recovered after a client crash follows the entry id across another
/// client's rename and reuse of its old name. A stationary file controls three moved trials.
// contract: C4a, C7, C12
#[test]
fn crash_recovery_follows_a_held_recreated_file() -> Result<()> {
    let Some((control, None)) = recover_held(0, false)? else {
        return Ok(());
    };
    assert_bytes(&control, b"recovered rewrite", "control recovery");
    for trial in 0..TRIALS {
        let Some((recovered, replacement)) = recover_held(trial, true)? else {
            return Ok(());
        };
        assert_bytes(&recovered, b"recovered rewrite", "recovered held entry");
        assert_bytes(
            &replacement.expect("moved trial has a replacement"),
            b"replacement bytes",
            "replacement after recovery",
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn held_symlink(trial: usize, moved: bool) -> Result<Option<PathBuf>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let old = format!("link-{moved}-{trial}");
    let relocated = format!("link-moved-{moved}-{trial}");
    std::os::unix::fs::symlink("original-target", a.join(&old))?;
    thread::sleep(VISIBLE_AFTER);
    let path = CString::new(b.join(&old).as_os_str().as_bytes())?;
    // SAFETY: `path` is NUL-terminated; a successful descriptor is owned immediately.
    let raw = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    ensure!(raw >= 0, "open held symlink: {}", std::io::Error::last_os_error());
    // SAFETY: `raw` is a new descriptor returned by open above.
    let held = unsafe { OwnedFd::from_raw_fd(raw) };
    if moved {
        fs::rename(a.join(&old), a.join(relocated))?;
        std::os::unix::fs::symlink("replacement-target", a.join(&old))?;
    }
    thread::sleep(VISIBLE_AFTER);

    let mut target = vec![0_u8; 256];
    // SAFETY: the empty path is NUL-terminated and `target` is writable for its full capacity.
    let len = unsafe {
        libc::readlinkat(
            held.as_raw_fd(),
            c"".as_ptr(),
            target.as_mut_ptr().cast(),
            target.len(),
        )
    };
    ensure!(
        len >= 0,
        "readlinkat held symlink: {}",
        std::io::Error::last_os_error()
    );
    target.truncate(len as usize);
    Ok(Some(PathBuf::from(OsString::from_vec(target))))
}

/// Readlink through a held symlink inode follows that entry after its old name is recreated. A
/// stationary symlink controls three rename-and-recreate trials.
// contract: C7
#[test]
#[cfg(target_os = "linux")] // O_PATH can hold a symlink inode; Darwin has no equivalent operation.
fn readlink_follows_a_held_recreated_symlink() -> Result<()> {
    let Some(control) = held_symlink(0, false)? else {
        return Ok(());
    };
    ensure!(control.as_os_str() == "original-target", "control target");
    for trial in 0..TRIALS {
        let Some(target) = held_symlink(trial, true)? else {
            return Ok(());
        };
        ensure!(
            target.as_os_str() == "original-target",
            "trial {trial}: {target:?}"
        );
    }
    Ok(())
}

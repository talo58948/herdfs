//! Round 6: lock files, which git and other tools take with O_CREAT|O_EXCL, fill, and rename
//! over the file they guard.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::thread;

use anyhow::{Result, ensure};

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, save, two_clients};

const NEW_LEN: usize = 64 << 10;

/// Tries to take `lock` as git does, and says whether another holds it.
fn lock_taken_elsewhere(lock: &Path) -> Result<bool> {
    match OpenOptions::new().write(true).create_new(true).open(lock) {
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(true),
        taken => {
            drop(taken?);
            Ok(false)
        }
    }
}

/// Repro: a saves `index` and b reads it. a takes `index.lock` with O_CREAT|O_EXCL and writes
/// new bytes into it; b tries to take the lock too and gets EEXIST, as a second git does. a closes
/// the lock and renames it over `index`, which commits it first (round 5), and b reads `index`
/// whole, the new bytes. b tries the lock again, gets EEXIST, and reads `index` again: 0 bytes.
/// An O_EXCL open makes the kernel look `index.lock` up again even while its name is cached, and
/// b's client answers from the entry it cached at the first try, before a's commit: the entry now
/// at `index`, with no version and size 0 (fs/mod.rs `entry`). `looked_up` sees that the inode's
/// path was confirmed since and keeps it, but still replies with the stale entry's attributes, and
/// the kernel takes size 0 for the inode that both names share. Without the second try b keeps
/// reading the new bytes. `git status` on b, which takes `index.lock` to refresh the index, fails
/// with "index file smaller than expected" while a commits.
// contract: C3, C7 (a rename replaces a file whole; a read never returns a version not committed)
#[test]
fn a_lock_renamed_over_a_file_never_makes_it_read_empty() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let (old, new) = (b"old index".to_vec(), random_bytes(NEW_LEN));
    save(&a.join("index"), &old)?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(&fs::read(b.join("index"))?, &old, "b's first read");

    let mut lock = File::options()
        .write(true)
        .create_new(true)
        .open(a.join("index.lock"))?;
    lock.write_all(&new)?;
    ensure!(lock_taken_elsewhere(&b.join("index.lock"))?, "b took a's lock");
    drop(lock);
    fs::rename(a.join("index.lock"), a.join("index"))?;
    assert_bytes(&fs::read(b.join("index"))?, &new, "b's read after the rename");

    let taken = lock_taken_elsewhere(&b.join("index.lock"))?;
    let read = fs::read(b.join("index"))?;
    assert!(
        read == old || read == new,
        "after a lock try that found it {}, b read {} bytes of `index`, neither the old {} nor the new {}",
        if taken { "taken" } else { "free" },
        read.len(),
        old.len(),
        new.len()
    );
    Ok(())
}

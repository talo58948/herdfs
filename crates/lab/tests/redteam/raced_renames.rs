//! Round 13: a rename on one client while other processes on the same client reopen the file being
//! renamed.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use herdfs_core::proto::Request;
use herdfs_lab::{ADMIN, SPACE};

use crate::{VISIBLE_AFTER, random_bytes, save, two_clients};

/// Chunks in the file's version, so that the reply to each open, which lists them, takes the
/// client a while to read: a 2 GiB file, well within the documented limit (a file written through
/// a mount has as many chunks at 2 to 8 GiB).
const CHUNKS: usize = 8_000;
/// One chunk: files below the 256 KiB minimum chunk are a single chunk.
const CHUNK: usize = 256 << 10;
const ROUNDS: usize = 20;
/// Threads on b that keep reopening the file.
const REOPENERS: usize = 4;
/// How long after each rename b keeps looking for the old name: within T_vis of any answer asked
/// for before the rename.
const LOOK_FOR: Duration = Duration::from_millis(900);
const LOOK_EVERY: Duration = Duration::from_millis(10);

/// a saves `f0`, and the admin commits a version of it that repeats its one chunk 8,000 times. b
/// opens `f0` and keeps the descriptor; with `reopen`, four other threads on b keep opening the
/// file again through that descriptor (`/proc/self/fd`), as programs that reread an input do. b
/// renames the file from `f0` to `f1`, then to `f2`, and so on, 20 times, and after each rename
/// looks for the old name for 0.9 s. Returns after how many renames b found the file at its old
/// name.
fn old_names_after_renames(reopen: bool) -> Result<Option<usize>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    save(&lab.mount_path("a").join("f0"), &random_bytes(CHUNK))?;
    let first = lab.head(ADMIN, "f0")?;
    let commit = Request::Commit {
        space: SPACE.into(),
        entry: first.entry,
        request_id: "many-chunks".into(),
        base: Some(first.id),
        guarded: false,
        chunks: vec![first.chunks[0]; CHUNKS],
        mtime_ms: None,
    };
    lab.rpc(ADMIN, &commit)?;
    thread::sleep(VISIBLE_AFTER);

    let b = lab.mount_path("b");
    let held = File::open(b.join("f0"))?;
    let again = format!("/proc/self/fd/{}", held.as_raw_fd());
    let stop = AtomicBool::new(false);
    let reopen_until_stopped = || {
        while !stop.load(Relaxed) {
            let _ = File::open(&again);
        }
    };
    thread::scope(|scope| {
        for _ in 0..if reopen { REOPENERS } else { 0 } {
            scope.spawn(reopen_until_stopped);
        }
        let mut found = 0;
        for round in 0..ROUNDS {
            let from = b.join(format!("f{round}"));
            fs::rename(&from, b.join(format!("f{}", round + 1)))?;
            found += usize::from(appears_within(LOOK_FOR, || fs::symlink_metadata(&from).is_ok()));
        }
        stop.store(true, Relaxed);
        Ok(Some(found))
    })
}

/// Whether `found` holds at some point within `span`, looking every `LOOK_EVERY`.
fn appears_within(span: Duration, found: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < span {
        if found() {
            return true;
        }
        thread::sleep(LOOK_EVERY);
    }
    false
}

/// Repro: [`old_names_after_renames`] with the reopening threads. A reopen asks the authority for
/// the file's head at its path before b's rename reaches the authority, and reads the reply's long
/// chunk list after the rename has moved the inode's path (fs/inodes.rs `moved`, which keeps the
/// time the old binding was confirmed). The reopen then takes the reply, which names the old path,
/// as newer than that binding (fs/ask.rs `head`, fs/inodes.rs `observe` and `bind`): it binds the
/// old name to the file again and marks the entry fresh, undoing the rename's `stale`, so a lookup
/// of the name b itself just moved the file away from finds the file there, after `rename`
/// returned. The control is [`a_renamed_file_is_gone_from_its_old_name`], without the reopening
/// threads.
// contract: C7 (rename is atomic within a space)
#[test]
#[ignore = "ROUND13: a reopen answered after a rename binds the file to its old name again"]
fn a_renamed_file_is_gone_from_its_old_name_while_it_is_reopened() -> Result<()> {
    let Some(found) = old_names_after_renames(true)? else {
        return Ok(());
    };
    assert_eq!(
        found, 0,
        "renames after which b still found the old name, of {ROUNDS}"
    );
    Ok(())
}

// contract: C7
#[test]
fn a_renamed_file_is_gone_from_its_old_name() -> Result<()> {
    let Some(found) = old_names_after_renames(false)? else {
        return Ok(());
    };
    assert_eq!(
        found, 0,
        "renames after which b still found the old name, of {ROUNDS}"
    );
    Ok(())
}

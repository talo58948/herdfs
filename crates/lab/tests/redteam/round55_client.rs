//! Round 55: held-child moves, duplicated truncate descriptors, and zero-capacity reads.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::Barrier;
use std::thread;

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, Links};

use crate::{
    VISIBLE_AFTER, assert_bytes, cache_lab, open_entry, random_bytes, read_at, read_entry, save,
    stat_count, two_clients, wait_pending,
};
use rustix::fs::OFlags;

const TRIALS: usize = 3;

fn write_below_moved_parent(
    lab: &Lab,
    trial: usize,
    move_child: bool,
) -> Result<(std::io::Result<()>, Vec<u8>, Vec<u8>)> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let parent = format!("split-{move_child}-{trial}");
    let moved = format!("split-moved-{move_child}-{trial}");
    let outside = format!("split-outside-{move_child}-{trial}");
    fs::create_dir(a.join(&parent))?;
    fs::create_dir(a.join(&outside))?;
    save(&a.join(&parent).join("child"), b"original")?;
    thread::sleep(VISIBLE_AFTER);

    let held = File::open(b.join(&parent))?;
    assert_bytes(&read_entry(&held, "child")?, b"original", "cached child");

    let original = if move_child {
        fs::rename(a.join(&parent).join("child"), a.join(&outside).join("child"))?;
        save(&a.join(&parent).join("child"), b"replaced")?;
        a.join(&outside).join("child")
    } else {
        a.join(&moved).join("child")
    };
    fs::rename(a.join(&parent), a.join(&moved))?;
    thread::sleep(VISIBLE_AFTER);
    held.metadata()?;

    lab.cut("b", Links::Authority)?;
    let written = open_entry(&held, "child", OFlags::WRONLY)
        .and_then(|mut child| child.write_all(b"OFFLINE!"));
    lab.restore("b", Links::Authority)?;
    if written.is_ok() {
        lab.wait_committed("b")?;
        thread::sleep(VISIBLE_AFTER);
    }
    Ok((
        written,
        fs::read(original)?,
        fs::read(a.join(moved).join("child"))?,
    ))
}

/// A child that moves with its held parent controls three cases where the held child first moves
/// out and is replaced. Its cached inode must still reach that child's new path while offline.
// contract: C7, C12
#[test]
fn an_offline_held_child_follows_its_own_move_before_its_parent_moves() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (written, original, child) = write_below_moved_parent(&lab, 0, false)?;
    written?;
    assert_bytes(&original, b"OFFLINE!", "control child");
    assert_bytes(&child, b"OFFLINE!", "control path");

    for trial in 0..TRIALS {
        let (written, original, replacement) = write_below_moved_parent(&lab, trial, true)?;
        written?;
        assert_bytes(&original, b"OFFLINE!", "independently moved child");
        assert_bytes(&replacement, b"replaced", "replacement child");
    }
    Ok(())
}

fn recover_after_one_duplicate_closes(lab: &mut Lab, trial: usize, crash: bool) -> Result<Vec<u8>> {
    let (a, b) = (lab.mount_path("a").to_owned(), lab.mount_path("b").to_owned());
    let name = format!("duplicate-truncate-{crash}-{trial}");
    save(&a.join(&name), b"old bytes")?;
    thread::sleep(VISIBLE_AFTER);
    fs::metadata(b.join(&name))?;

    lab.cut("b", Links::Authority)?;
    let first = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(b.join(&name))?;
    let duplicate = first.try_clone()?;
    drop(first);
    wait_pending(lab)?;
    if crash {
        lab.kill_client("b")?;
        drop(duplicate);
        lab.restore("b", Links::Authority)?;
        lab.restart_client("b")?;
    } else {
        drop(duplicate);
        lab.restore("b", Links::Authority)?;
    }
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    Ok(fs::read(a.join(name))?)
}

/// Closing both duplicate descriptors and reconnecting controls three crashes after only one
/// close. The close-returned truncate must already have a durable deferred seal.
// contract: C4a, C12
#[test]
fn a_truncate_survives_a_crash_after_one_duplicate_descriptor_closes() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    ensure!(recover_after_one_duplicate_closes(&mut lab, 0, false)?.is_empty());
    for trial in 0..TRIALS {
        ensure!(recover_after_one_duplicate_closes(&mut lab, trial, true)?.is_empty());
    }
    Ok(())
}

fn concurrent_first_byte(file: &File, expected: u8) -> Result<()> {
    const READERS: usize = 8;
    let start = Barrier::new(READERS);
    thread::scope(|scope| {
        let reads = (0..READERS)
            .map(|_| {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    read_at(file, 0, 1)
                })
            })
            .collect::<Vec<_>>();
        for read in reads {
            let bytes = read.join().map_err(|_| anyhow::anyhow!("reader panicked"))??;
            ensure!(bytes == [expected], "concurrent read returned wrong byte");
        }
        Ok(())
    })
}

/// One cold read controls three zero-capacity waves of eight reads of the same chunk. Each read
/// may fetch that one chunk, but eviction races must not change bytes or discard the live grant.
// contract: C8, C9, C11, S2
#[test]
fn zero_capacity_concurrent_reads_stay_correct_and_reuse_the_live_grant() -> Result<()> {
    const READERS: u64 = 8;
    let Some(lab) = cache_lab(0)? else {
        return Ok(());
    };
    let bytes = random_bytes(1 << 20);
    save(&lab.mount_path("a").join("zero-capacity"), &bytes)?;
    thread::sleep(VISIBLE_AFTER);
    let file = File::open(lab.mount_path("b").join("zero-capacity"))?;
    assert_bytes(&read_at(&file, 0, 1)?, &bytes[..1], "cold control");
    let grants = stat_count(&lab, "grants")?;
    ensure!(grants > 0, "cold control requested no grant");

    for trial in 0..TRIALS {
        let before = stat_count(&lab, "chunks_fetched")?;
        concurrent_first_byte(&file, bytes[0])?;
        let fetched = stat_count(&lab, "chunks_fetched")? - before;
        ensure!(
            (1..=READERS).contains(&fetched),
            "trial {trial} fetched {fetched} chunks"
        );
        ensure!(
            stat_count(&lab, "grants")? == grants,
            "trial {trial} renewed a live grant"
        );
    }
    Ok(())
}

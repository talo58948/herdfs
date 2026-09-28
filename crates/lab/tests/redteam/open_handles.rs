//! Files that stay open on one client while another client changes them.

use std::fs::{self, File};
use std::thread;

use anyhow::Result;

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, read_at, save, two_clients};

const LEN: usize = 1 << 20;
const PAGE: usize = 4096;

/// Repro: on b, open the file at v1, let a commit v2, open it again on b, read a range through
/// the first handle, then the same range through the second. The first read refills the kernel's
/// page cache, which is per inode, with v1 pages, and the second handle is served those.
// contract: C2, C3
#[test]
#[ignore = "REDTEAM: a later open reads the older version's pages another open on its client cached"]
fn an_open_after_a_commit_never_reads_the_older_version() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (old, new) = (vec![b'1'; LEN], vec![b'2'; LEN]);
    save(&lab.mount_path("a").join("f"), &old)?;
    thread::sleep(VISIBLE_AFTER);
    let path = lab.mount_path("b").join("f");
    let early = File::open(&path)?;
    assert_bytes(&read_at(&early, 0, PAGE)?, &old[..PAGE], "the first open");

    save(&lab.mount_path("a").join("f"), &new)?;
    thread::sleep(VISIBLE_AFTER);
    let late = File::open(&path)?;
    let middle = (LEN / 2) as u64;
    read_at(&early, middle, PAGE)?;
    assert_bytes(
        &read_at(&late, middle, PAGE)?,
        &new[..PAGE],
        "an open that began after the commit",
    );
    Ok(())
}

/// Repro: on b, open a file of several chunks and read its start; on a, rename it; on b, read
/// its end through the same handle. Grants name the path the handle was opened at, which no
/// longer holds the entry, so the read fails although the entry and its version still exist.
// contract: C7, C14
#[test]
#[ignore = "REDTEAM: an open file renamed on another client fails reads of uncached ranges"]
fn an_open_file_stays_readable_after_another_client_renames_it() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let bytes = random_bytes(12 << 20);
    save(&lab.mount_path("a").join("f"), &bytes)?;
    thread::sleep(VISIBLE_AFTER);
    let file = File::open(lab.mount_path("b").join("f"))?;
    assert_bytes(&read_at(&file, 0, PAGE)?, &bytes[..PAGE], "the start");

    fs::rename(lab.mount_path("a").join("f"), lab.mount_path("a").join("g"))?;
    let tail = bytes.len() - PAGE;
    let read = read_at(&file, tail as u64, PAGE);
    assert!(read.is_ok(), "reading the end after the rename: {read:?}");
    assert_bytes(&read?, &bytes[tail..], "the end");
    Ok(())
}

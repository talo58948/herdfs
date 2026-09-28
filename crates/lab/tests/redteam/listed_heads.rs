//! Round 11: an open file on a client whose stat of it is answered from a directory listing that
//! already names a newer version.

use std::fs::{self, File};
use std::thread;

use anyhow::{Result, ensure};

use crate::{VISIBLE_AFTER, random_bytes, read_at, save, two_clients};

/// Several chunks, so a read of the first page leaves the last one uncached.
const LEN: usize = 4 << 20;
const PAGE: usize = 4096;

/// a saves v1 of `f`; b opens it and reads its first page. a saves v2, of the same length. After
/// T_vis, with `list`, b lists the mount's root, as `ls -l` does before it stats each name; then
/// b stats `f`, which reports v2's modification time. Returns which version b's open descriptor
/// then reads at the last page: `v1`, `v2` or `neither`.
fn read_after_a_stat(list: bool) -> Result<Option<&'static str>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let (on_a, on_b) = (lab.mount_path("a").join("f"), lab.mount_path("b").join("f"));
    let (v1, v2) = (random_bytes(LEN), random_bytes(LEN));
    save(&on_a, &v1)?;
    thread::sleep(VISIBLE_AFTER);
    let file = File::open(&on_b)?;
    ensure!(read_at(&file, 0, PAGE)? == v1[..PAGE], "b reads v1 first");
    let v1_time = fs::metadata(&on_b)?.modified()?;

    save(&on_a, &v2)?;
    thread::sleep(VISIBLE_AFTER);
    if list {
        for listed in fs::read_dir(lab.mount_path("b"))? {
            listed?;
        }
    }
    ensure!(
        fs::metadata(&on_b)?.modified()? != v1_time,
        "b's stat reports v2's modification time"
    );
    let last = read_at(&file, (LEN - PAGE) as u64, PAGE)?;
    Ok(Some(if last == v2[LEN - PAGE..] {
        "v2"
    } else if last == v1[LEN - PAGE..] {
        "v1"
    } else {
        "neither"
    }))
}

/// Repro: [`read_after_a_stat`] with the listing. The listing caches every entry it names, and
/// the stat takes `f`'s from there (fs/paths.rs `entry` and `cached`), so it never reaches the
/// step that switches an open file to a newer head (fs/versions.rs `follow`, which only a fetch
/// runs). b's stat reports v2 while its descriptor goes on reading v1, until the cached entry
/// expires and a later stat fetches it. Once a getattr sees a newer head, every handle on the
/// inode reads it from its next read (docs/architecture.md, C3). The control is
/// [`an_open_file_reads_the_version_its_stat_reported`], without the listing, where the stat
/// fetches the entry and the descriptor reads v2.
// contract: C3 (docs/architecture.md: once a getattr sees a newer head, every handle on the inode
// reads the new version from its next read)
#[test]
#[ignore = "ROUND11: a stat answered from a listing reports a new version, open files keep the old"]
fn an_open_file_reads_the_version_its_stat_reported_after_a_listing() -> Result<()> {
    let Some(read) = read_after_a_stat(true)? else {
        return Ok(());
    };
    assert_eq!(read, "v2", "b's read of the last page");
    Ok(())
}

// contract: C3
#[test]
fn an_open_file_reads_the_version_its_stat_reported() -> Result<()> {
    let Some(read) = read_after_a_stat(false)? else {
        return Ok(());
    };
    assert_eq!(read, "v2", "b's read of the last page");
    Ok(())
}

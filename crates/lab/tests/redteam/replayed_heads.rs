//! Round 10: a file open on the client whose commit answer was lost, when the retried commit is
//! answered with the head another client committed since, because a purge removed the version
//! the first try made.

use std::fs::{self, File};
use std::thread;
use std::time::Duration;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::Request;
use herdfs_lab::{ADMIN, Lab, LabConfig, Links, Profile, SPACE, Strategy};

use crate::{SLOW_ANSWERS, VISIBLE_AFTER, random_bytes, read_at, save, wait_for_rewrite};

/// Several chunks, so a read of the first page leaves the last one uncached.
const LEN: usize = 4 << 20;
const PAGE: usize = 4096;
/// a's links: half a second each way, so the answer to a commit is still on its way when the
/// test sees the version at the authority and cuts a's links.
/// a saves v1 of `f`, then closes a rewrite of it without fsync, opens `f` again for reading and
/// reads its first page, which is the rewrite's. The moment the authority has committed the
/// rewrite, and before its answer crosses a's slow link, the test cuts a's links, so a's committer
/// retries. b saves `f`; with `purge`, the admin purges `f`'s history, which removes the rewrite's
/// version. The test restores a's links, waits until a has committed, then after T_vis stats `f`
/// on a, as `ls -l` does, which sees b's save as the head. Returns what a's open descriptor reads
/// then at the first and the last page: `b's save`, `a's rewrite` or `neither` for each.
fn read_after_a_retried_commit(purge: bool) -> Result<Option<[&'static str; 2]>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], SLOW_ANSWERS, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream);
    let lab = Lab::start(config)?;
    let (on_a, on_b) = (lab.mount_path("a").join("f"), lab.mount_path("b").join("f"));
    save(&on_a, &random_bytes(LEN))?;
    let v1 = lab.head(ADMIN, "f")?;
    let (rewrite, saved) = (random_bytes(LEN), random_bytes(LEN));
    fs::write(&on_a, &rewrite)?;
    let file = File::open(&on_a)?;
    ensure!(
        read_at(&file, 0, PAGE)? == rewrite[..PAGE],
        "a reads its rewrite before it commits"
    );
    wait_for_rewrite(&lab, "f", &v1)?;
    lab.cut("a", Links::Both)?;
    thread::sleep(Duration::from_millis(500));
    ensure!(
        lab.stats("a")?["pending_commits"] == 1,
        "a settled the rewrite before its links were cut"
    );

    save(&on_b, &saved)?;
    let head = lab.head(ADMIN, "f")?.id;
    if purge {
        let purge = Request::Purge {
            space: SPACE.into(),
            path: SpacePath::try_from("f")?,
        };
        lab.rpc(ADMIN, &purge)?;
    }
    lab.restore("a", Links::Both)?;
    lab.wait_committed("a")?;
    ensure!(lab.head(ADMIN, "f")?.id == head, "b's save stays the head");
    thread::sleep(VISIBLE_AFTER);
    fs::metadata(&on_a)?;
    let which = |off: usize| -> Result<&'static str> {
        let page = read_at(&file, off as u64, PAGE)?;
        Ok(if page == saved[off..off + PAGE] {
            "b's save"
        } else if page == rewrite[off..off + PAGE] {
            "a's rewrite"
        } else {
            "neither"
        })
    };
    // The last page first: the first is the one the kernel may still cache.
    let last = which(LEN - PAGE)?;
    Ok(Some([which(0)?, last]))
}

/// Repro: [`read_after_a_retried_commit`] with the purge. The retried commit finds the request id
/// of the first try, whose version is gone, and is answered with the head, b's save; the committer
/// makes that the version a's open files read (journal/commit.rs `commit`), but never drops the
/// kernel's pages of the file, which hold the rewrite a read through its journal copy. The stat finds b's save already
/// followed and drops nothing either. So the descriptor reads b's save at the last page and a's
/// rewrite at the first, for as long as it stays open: a mix of two versions, long after the
/// switch. The control is [`an_open_file_reads_the_head_after_a_retried_commit_of_a_kept_version`],
/// without the purge, where the retry is answered with the rewrite and the stat moves the file
/// on to b's save.
// contract: C3
#[test]
fn an_open_file_reads_the_head_after_a_retried_commit_of_a_purged_version() -> Result<()> {
    let Some(read) = read_after_a_retried_commit(true)? else {
        return Ok(());
    };
    assert_eq!(read, ["b's save"; 2], "the first and the last page on a");
    Ok(())
}

// contract: C3
#[test]
fn an_open_file_reads_the_head_after_a_retried_commit_of_a_kept_version() -> Result<()> {
    let Some(read) = read_after_a_retried_commit(false)? else {
        return Ok(());
    };
    assert_eq!(read, ["b's save"; 2], "the first and the last page on a");
    Ok(())
}

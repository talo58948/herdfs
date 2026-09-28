//! Round 10: a file open on the client whose pending write the authority refuses, because the
//! writer lost write access between its open for writing and its close.

use std::fs::{self, File};
use std::io::Write;
use std::thread;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, SPACE};

use crate::{VISIBLE_AFTER, random_bytes, read_at, save, two_clients};

/// Several chunks, so a read of the first page leaves the last one uncached.
const LEN: usize = 4 << 20;
const PAGE: usize = 4096;

/// a saves v1 of `f` and opens it for writing with O_TRUNC, writes a rewrite through that
/// descriptor, then opens `f` again for reading and reads its first page, which is the rewrite's.
/// With `revoke`, the admin narrows alice to read on the whole space before a closes the writer,
/// so the authority refuses the commit. After a has settled it and T_vis has passed, the test
/// stats `f` on a, as `ls -l` does. Returns what the reading descriptor then reads at the first
/// and the last page: `v1`, `the rewrite` or `neither` for each.
fn read_after_a_settled_write(revoke: bool) -> Result<Option<[&'static str; 2]>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let path = lab.mount_path("a").join("f");
    let (v1, rewrite) = (random_bytes(LEN), random_bytes(LEN));
    save(&path, &v1)?;
    let mut writer = File::create(&path)?;
    writer.write_all(&rewrite)?;
    let file = File::open(&path)?;
    ensure!(
        read_at(&file, 0, PAGE)? == rewrite[..PAGE],
        "a reads its rewrite before it commits"
    );
    if revoke {
        let read_only = Request::SetAcl {
            space: SPACE.into(),
            path: SpacePath::root(),
            principal: "user:alice".into(),
            perm: Some(Perm::Read),
        };
        lab.rpc(ADMIN, &read_only)?;
    }
    drop(writer);
    lab.wait_committed("a")?;
    // A change of size would make the kernel drop the pages itself.
    ensure!(
        lab.head(ADMIN, "f")?.size == LEN as u64,
        "the head has the length of both writes"
    );

    thread::sleep(VISIBLE_AFTER);
    fs::metadata(&path)?;
    let which = |off: usize| -> Result<&'static str> {
        let page = read_at(&file, off as u64, PAGE)?;
        Ok(if page == v1[off..off + PAGE] {
            "v1"
        } else if page == rewrite[off..off + PAGE] {
            "the rewrite"
        } else {
            "neither"
        })
    };
    // The last page first: the first is the one the kernel may still cache.
    let last = which(LEN - PAGE)?;
    let pages = [which(0)?, last];
    let expected = if revoke { &v1 } else { &rewrite };
    // A new open drops the kernel's pages, so it comes last.
    ensure!(
        fs::read(&path)? == *expected,
        "a new open on a reads the head whole"
    );
    Ok(Some(pages))
}

/// Repro: [`read_after_a_settled_write`] with the revocation. The commit is refused and set
/// aside, and the committer drops the journal copy (journal/commit.rs `finish`), so a's open files
/// read v1 again, but nothing drops the kernel's pages of the file, which hold the rewrite the
/// reader read through that copy; the stat finds v1 still followed and drops nothing either. So
/// the descriptor reads v1 at the last page and the refused rewrite, which no version holds, at
/// the first, for as long as it stays open, while a new open reads v1 whole. The control is
/// [`an_open_file_reads_the_head_after_its_clients_write_commits`], where the commit succeeds.
// contract: C3
#[test]
fn an_open_file_reads_the_head_after_its_clients_write_is_refused() -> Result<()> {
    let Some(read) = read_after_a_settled_write(true)? else {
        return Ok(());
    };
    assert_eq!(read, ["v1"; 2], "the first and the last page on a");
    Ok(())
}

// contract: C3, C1
#[test]
fn an_open_file_reads_the_head_after_its_clients_write_commits() -> Result<()> {
    let Some(read) = read_after_a_settled_write(false)? else {
        return Ok(());
    };
    assert_eq!(read, ["the rewrite"; 2], "the first and the last page on a");
    Ok(())
}

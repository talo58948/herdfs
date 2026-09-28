//! Round 11: fsync on a descriptor whose write another descriptor's close sealed, once the
//! authority has refused the commit that carried it.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, SPACE};

use crate::{assert_bytes, save, two_clients};

/// a saves `f`, then opens it twice for writing, as two writers of one file do, and writes
/// through each descriptor. With `revoke`, the admin narrows alice to read on the whole space.
/// a closes the second descriptor, which seals both writes, and waits until its committer has
/// settled that commit. Returns what fsync on the first descriptor, still open, then returns,
/// and the head's bytes as b reads them.
fn fsync_after_another_close(revoke: bool) -> Result<Option<(io::Result<()>, Vec<u8>)>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let path = lab.mount_path("a").join("f");
    save(&path, b"0123456789")?;
    let first = File::options().write(true).open(&path)?;
    first.write_all_at(b"AB", 0)?;
    let second = File::options().write(true).open(&path)?;
    second.write_all_at(b"cd", 4)?;
    if revoke {
        let read_only = Request::SetAcl {
            space: SPACE.into(),
            path: SpacePath::root(),
            principal: "user:alice".into(),
            perm: Some(Perm::Read),
        };
        lab.rpc(ADMIN, &read_only)?;
    }
    drop(second);
    lab.wait_committed("a")?;
    ensure!(
        lab.stats("a")?["pending_commits"] == 0,
        "a settled the commit before the fsync"
    );
    let synced = first.sync_all();
    // A fresh open on b reads the head at the authority.
    let head = fs::read(lab.mount_path("b").join("f"))?;
    Ok(Some((synced, head)))
}

/// Repro: [`fsync_after_another_close`] with the revocation. The second close seals the journal
/// copy, which holds both descriptors' writes, and the authority refuses its commit, which the
/// committer sets aside. fsync on the first descriptor then finds nothing written since the seal
/// and no commit pending (fs/handles.rs `fsync` and `committed`), and returns 0, so the program
/// is told its write reached the authority, which never took it and still holds the old bytes.
/// fsync "returns EIO if [the commit] fails" (docs/architecture.md), as a local disk's fsync
/// reports a failed writeback to every descriptor open when it failed. The control is
/// [`fsync_after_another_close_that_commits`], where the commit succeeds and fsync returns 0.
// contract: C2 (docs/architecture.md: fsync waits for that commit and returns EIO if it fails)
#[test]
#[ignore = "ROUND11: fsync returns 0 for a write another close sealed and the authority refused"]
fn fsync_reports_a_refused_commit_that_another_close_started() -> Result<()> {
    let Some((synced, head)) = fsync_after_another_close(true)? else {
        return Ok(());
    };
    assert_bytes(&head, b"0123456789", "the head at the authority");
    let errno = synced.err().and_then(|error| error.raw_os_error());
    assert_eq!(errno, Some(libc::EIO), "fsync on the first descriptor");
    Ok(())
}

// contract: C2
#[test]
fn fsync_after_another_close_that_commits() -> Result<()> {
    let Some((synced, head)) = fsync_after_another_close(false)? else {
        return Ok(());
    };
    synced?;
    assert_bytes(&head, b"AB23cd6789", "the head at the authority");
    Ok(())
}

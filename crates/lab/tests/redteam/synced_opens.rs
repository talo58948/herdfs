//! Round 12: fsync on a descriptor opened after a close sealed a write, when the authority refuses
//! that write's commit while the descriptor is open.

use std::fs::{self, File};
use std::io;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, SPACE};

use crate::{assert_bytes, save, two_clients};

/// a saves `f`, then rewrites it and closes without fsync, so the rewrite waits out the
/// committer's quiet period. Another descriptor opens `f` for reading, as `sync f` does. With
/// `revoke`, the admin then narrows alice to read on the whole space, so the authority refuses
/// the rewrite's commit while that descriptor is open. Once a has settled the commit, returns what
/// fsync on the descriptor returns, and the head's bytes as b reads them.
fn fsync_after_a_commit_settles(revoke: bool) -> Result<Option<(io::Result<()>, Vec<u8>)>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let path = lab.mount_path("a").join("f");
    save(&path, b"old")?;
    fs::write(&path, b"first")?;
    let held = File::open(&path)?;
    if revoke {
        let read_only = Request::SetAcl {
            space: SPACE.into(),
            path: SpacePath::root(),
            principal: "user:alice".into(),
            perm: Some(Perm::Read),
        };
        lab.rpc(ADMIN, &read_only)?;
    }
    ensure!(
        lab.stats("a")?["pending_commits"] == 1,
        "the rewrite's commit is still pending after the open"
    );
    lab.wait_committed("a")?;
    let synced = held.sync_all();
    let head = fs::read(lab.mount_path("b").join("f"))?;
    Ok(Some((synced, head)))
}

/// Repro: [`fsync_after_a_commit_settles`] with the revocation. The descriptor opens while the
/// rewrite's commit is pending, and the authority refuses that commit while it is open. fsync then
/// finds nothing pending and compares the refused seal with the seal count taken at open
/// (fs/handles.rs `fsync`, `Handle::new`); both are the rewrite's seal, so fsync returns 0 though
/// the commit it covers failed after the open and the authority still holds "old". fsync "waits for
/// that commit and returns EIO if it fails" (docs/architecture.md), as a local disk reports a
/// failed writeback to every descriptor open when it failed. The control is
/// [`fsync_after_a_commit_that_lands`], where the commit succeeds and fsync returns 0.
// contract: C2 (docs/architecture.md: fsync waits for that commit and returns EIO if it fails)
#[test]
#[ignore = "ROUND12: fsync returns 0 on a descriptor opened before a refusal of an earlier seal"]
fn fsync_reports_a_commit_refused_while_the_descriptor_was_open() -> Result<()> {
    let Some((synced, head)) = fsync_after_a_commit_settles(true)? else {
        return Ok(());
    };
    assert_bytes(&head, b"old", "the head at the authority");
    let errno = synced.err().and_then(|error| error.raw_os_error());
    assert_eq!(errno, Some(libc::EIO), "fsync on the descriptor");
    Ok(())
}

// contract: C2
#[test]
fn fsync_after_a_commit_that_lands() -> Result<()> {
    let Some((synced, head)) = fsync_after_a_commit_settles(false)? else {
        return Ok(());
    };
    synced?;
    assert_bytes(&head, b"first", "the head at the authority");
    Ok(())
}

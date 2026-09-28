//! Round 11: an open for writing or a truncation that comes after the writer lost write access,
//! while an earlier write of the same file is still waiting for its commit.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, SPACE};

use crate::{assert_bytes, save, two_clients};

/// Overwrites the file at `path` with "second", as `echo second > f` does.
fn overwrite(path: &Path) -> io::Result<()> {
    File::create(path)?.write_all(b"second")
}

/// Empties the file at `path` by name, without opening it, as truncate(2) does.
#[expect(unsafe_code, reason = "a truncation by name is the behavior under test")]
fn truncate(path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    match unsafe { libc::truncate(path.as_ptr(), 0) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// a saves `f`; with `pending`, a then rewrites it and closes without fsync, so that write waits
/// out the committer's quiet period. The admin narrows alice to read on the whole space. a
/// changes `f` again with `change`. Once a has settled every commit, returns whether that change
/// succeeded, and the bytes a then reads.
fn change_after_revocation(
    pending: bool,
    change: fn(&Path) -> io::Result<()>,
) -> Result<Option<(bool, Vec<u8>)>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let path = lab.mount_path("a").join("f");
    save(&path, b"old")?;
    if pending {
        fs::write(&path, b"first")?;
    }
    let read_only = Request::SetAcl {
        space: SPACE.into(),
        path: SpacePath::root(),
        principal: "user:alice".into(),
        perm: Some(Perm::Read),
    };
    lab.rpc(ADMIN, &read_only)?;
    let waiting = if pending { 1 } else { 0 };
    ensure!(
        lab.stats("a")?["pending_commits"] == waiting,
        "a's first rewrite is still waiting for its commit"
    );
    let changed = change(&path);
    lab.wait_committed("a")?;
    Ok(Some((changed.is_ok(), fs::read(&path)?)))
}

/// Repro: [`change_after_revocation`] with the pending rewrite and [`overwrite`]. An open asks
/// the authority for the head, and with it for write access, only while the client holds no
/// changes of the file (fs/handles.rs `open`); the pending rewrite skips that, so the overwrite's
/// open, write and close all return 0 after the revocation. The authority then refuses its
/// commit, which is set aside, and a reads the old bytes: the write vanished though every call
/// reported success. "Every open asks the authority for the current head", and one for writing
/// needs write access (docs/architecture.md), which the round-1 fix relies on so that such a
/// write fails instead of vanishing (C1, C4a). The control is
/// [`an_overwrite_after_revocation_fails_at_open`], with no pending rewrite, where the open fails
/// with EACCES. A truncation by name skips the same check (fs/handles.rs `truncate`):
/// [`a_truncation_after_revocation_fails_while_an_earlier_write_is_pending`].
// contract: C1, C4a
#[test]
#[ignore = "ROUND11: an open for writing skips the write check while a write is pending"]
fn an_overwrite_after_revocation_fails_while_an_earlier_write_is_pending() -> Result<()> {
    let Some((written, read)) = change_after_revocation(true, overwrite)? else {
        return Ok(());
    };
    if written {
        assert_bytes(&read, b"second", "a's read after its overwrite returned 0");
    }
    Ok(())
}

// contract: C1, C4a
#[test]
fn an_overwrite_after_revocation_fails_at_open() -> Result<()> {
    let Some((written, read)) = change_after_revocation(false, overwrite)? else {
        return Ok(());
    };
    ensure!(!written, "the overwrite fails");
    assert_bytes(&read, b"old", "a's read after the failed overwrite");
    Ok(())
}

/// Repro: [`change_after_revocation`] with the pending rewrite and [`truncate`], which returns 0
/// after the revocation; its commit is refused and a reads the old bytes. The control is
/// [`a_truncation_after_revocation_fails`], where truncate fails with EACCES.
// contract: C1, C4a
#[test]
#[ignore = "ROUND11: a truncation by name skips the write check while a write is pending"]
fn a_truncation_after_revocation_fails_while_an_earlier_write_is_pending() -> Result<()> {
    let Some((truncated, read)) = change_after_revocation(true, truncate)? else {
        return Ok(());
    };
    if truncated {
        assert_bytes(&read, b"", "a's read after its truncation returned 0");
    }
    Ok(())
}

// contract: C1, C4a
#[test]
fn a_truncation_after_revocation_fails() -> Result<()> {
    let Some((truncated, read)) = change_after_revocation(false, truncate)? else {
        return Ok(());
    };
    ensure!(!truncated, "the truncation fails");
    assert_bytes(&read, b"old", "a's read after the failed truncation");
    Ok(())
}

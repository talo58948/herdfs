//! A client's journal copy against what the authority holds.

use std::fs::{self, File};
use std::io::Write;
use std::thread;

use anyhow::Result;
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, SPACE};

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

/// Repro: b opens a file for writing and writes nothing; a commits a new version; b opens the
/// file again read-only after T_vis. Opening for writing gave the entry a journal copy of the
/// old head, and every read on b goes to that copy while it exists, which is as long as any
/// handle is open, pending write or not.
// contract: C2
#[test]
fn an_idle_writer_does_not_hide_a_newer_commit() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join("f"), b"old")?;
    thread::sleep(VISIBLE_AFTER);
    let path = lab.mount_path("b").join("f");
    let _idle = File::options().read(true).write(true).open(&path)?;

    save(&lab.mount_path("a").join("f"), b"new")?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(&fs::read(&path)?, b"new", "b's read after a's commit");
    Ok(())
}

/// Repro: the admin narrows bob to read on the whole space; bob overwrites an existing file.
/// Open only asks for read, the write goes to the journal, and close returns 0; the commit is
/// refused, the write set aside under `journal/rejected/`, and bob then reads the old bytes.
// contract: C1, C4a
#[test]
fn a_write_without_write_access_fails_instead_of_vanishing() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join("f"), b"old")?;
    let read_only = Request::SetAcl {
        space: SPACE.into(),
        path: SpacePath::root(),
        principal: "user:bob".into(),
        perm: Some(Perm::Read),
    };
    lab.rpc(ADMIN, &read_only)?;
    thread::sleep(VISIBLE_AFTER);

    let path = lab.mount_path("b").join("f");
    let written = File::create(&path).and_then(|mut file| file.write_all(b"new"));
    lab.wait_committed("b")?;
    if written.is_ok() {
        assert_bytes(&fs::read(&path)?, b"new", "bob's read after his close returned 0");
    }
    Ok(())
}

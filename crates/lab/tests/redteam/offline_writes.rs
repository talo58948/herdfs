//! Round 3: offline writes that C12 covers, after what the client knows has aged or been bounded.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::thread;

use anyhow::Result;
use herdfs_core::SpacePath;
use herdfs_core::proto::{EntryId, Perm, Request, Response};
use herdfs_lab::{ADMIN, Links, SPACE};

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

/// More files than the client keeps a followed version for (reader.rs `HEADS`).
const MANY: usize = 4_200;

/// Repro: b opens `log` for appending, appends a line and fsyncs it, and keeps the descriptor
/// open, as a logger does; b then opens 4,200 other files one by one, as `grep -r` or a build
/// does; b loses its authority link and appends another line through the same descriptor. b
/// wrote every byte of `log` and caches them, but the bounded LRU of the versions open files
/// follow (reader.rs `HEADS`, 4,096) has dropped `log`'s although a descriptor is open on it, so
/// the append needs the head from the authority (handles.rs `prepare`) and fails with EIO. After
/// opening 100 files instead, the same append commits.
// contract: C12
#[test]
fn an_offline_append_through_a_descriptor_held_open_is_committed() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join("log"), b"first\n")?;
    let parent = mkdir(&lab, "many")?;
    for index in 0..MANY {
        let path = SpacePath::try_from(format!("many/{index}"))?;
        let (space, mode) = (SPACE.into(), 0o644);
        lab.rpc(
            ADMIN,
            &Request::Create {
                space,
                path,
                parent,
                mode,
            },
        )?;
    }
    thread::sleep(VISIBLE_AFTER);
    let b = lab.mount_path("b");
    let mut log = OpenOptions::new().append(true).open(b.join("log"))?;
    log.write_all(b"second\n")?;
    log.sync_all()?;
    for index in 0..MANY {
        File::open(b.join(format!("many/{index}")))?;
    }

    lab.cut("b", Links::Authority)?;
    let appended = log.write_all(b"third\n");
    lab.restore("b", Links::Authority)?;
    assert!(appended.is_ok(), "b's offline append: {appended:?}");
    drop(log);
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(
        &fs::read(lab.mount_path("a").join("log"))?,
        b"first\nsecond\nthird\n",
        "a's read",
    );
    Ok(())
}

/// Repro: the admin narrows bob to read on the whole space; b reads `f`, loses its authority
/// link and overwrites `f` (O_TRUNC, write, close). b's open of `f` was told b may not commit to
/// it (`writable: false`), but the client keeps only the version, so offline the open for writing
/// succeeds and close returns 0; on reconnect the commit is refused and set aside, and b reads the
/// old bytes again. Round 1 fixed the same write online.
// contract: C1, C4a, C12
#[test]
fn an_offline_write_without_write_access_fails_instead_of_vanishing() -> Result<()> {
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
    assert_bytes(&fs::read(&path)?, b"old", "b's read");

    lab.cut("b", Links::Authority)?;
    let written = File::create(&path).and_then(|mut file| file.write_all(b"new"));
    lab.restore("b", Links::Authority)?;
    lab.wait_committed("b")?;
    if written.is_ok() {
        let rejected = fs::read_dir(lab.client_dir("b").join("journal/rejected"))?.count();
        let what = format!("bob's read after his close returned 0 ({rejected} files set aside)");
        assert_bytes(&fs::read(&path)?, b"new", &what);
    }
    Ok(())
}

/// Repro: b reads `f`, loses its authority link, opens `f` for writing and truncates it to 0 with
/// ftruncate(2), as a program that rewrites a file in place does. The client journals the
/// truncation (handles.rs `truncate`), then asks the authority for the entry's attributes
/// (namespace.rs `setattr`), which fails offline: ftruncate returns EIO. The program takes the
/// file as untouched and closes the descriptor, and on reconnect `f` commits empty.
// contract: C12
#[test]
fn an_offline_truncate_that_fails_leaves_the_file_as_it_was() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join("f"), b"old")?;
    thread::sleep(VISIBLE_AFTER);
    let path = lab.mount_path("b").join("f");
    assert_bytes(&fs::read(&path)?, b"old", "b's read");

    lab.cut("b", Links::Authority)?;
    let file = OpenOptions::new().write(true).open(&path)?;
    let truncated = file.set_len(0);
    drop(file);
    lab.restore("b", Links::Authority)?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    let expected: &[u8] = if truncated.is_ok() { b"" } else { b"old" };
    let what = format!("a's read after b's offline ftruncate returned {truncated:?}");
    assert_bytes(&fs::read(lab.mount_path("a").join("f"))?, expected, &what);
    Ok(())
}

fn mkdir(lab: &herdfs_lab::Lab, at: &str) -> Result<EntryId> {
    let (space, path, mode) = (SPACE.into(), SpacePath::try_from(at)?, 0o755);
    let Response::Entry { entry: root } = lab.rpc(
        ADMIN,
        &Request::Stat {
            space: SPACE.into(),
            path: SpacePath::root(),
        },
    )?
    else {
        anyhow::bail!("Stat answered something else")
    };
    let Response::Entry { entry } = lab.rpc(
        ADMIN,
        &Request::Mkdir {
            space,
            path,
            parent: root.id,
            mode,
        },
    )?
    else {
        anyhow::bail!("Mkdir answered something else")
    };
    Ok(entry.id)
}

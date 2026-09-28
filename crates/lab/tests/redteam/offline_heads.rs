//! Round 2: offline writes to files whose current bytes the client holds (C12).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::thread;

use anyhow::Result;
use herdfs_core::SpacePath;
use herdfs_core::proto::{EntryId, Request, Response};
use herdfs_lab::{ADMIN, Lab, Links, SPACE};

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

/// More entries than a client keeps metadata for.
const MANY: usize = 4_200;

/// Appends `line` to `path` through a fresh open.
fn append(path: &Path, line: &[u8]) -> std::io::Result<()> {
    OpenOptions::new().append(true).open(path)?.write_all(line)
}

/// Appends `line` to b's `name` with b's authority link cut, then checks that a reads `expected`
/// once b has reconnected and committed.
fn append_offline(lab: &Lab, name: &str, line: &[u8], expected: &[u8]) -> Result<()> {
    lab.cut("b", Links::Authority)?;
    let appended = append(&lab.mount_path("b").join(name), line);
    lab.restore("b", Links::Authority)?;
    assert!(appended.is_ok(), "b's offline append: {appended:?}");
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(&fs::read(lab.mount_path("a").join(name))?, expected, "a's read");
    Ok(())
}

/// Repro: b creates `log` and saves a line, then loses its authority link and appends a line. b
/// wrote every byte of `log` and caches them, but it only remembers a head for opening offline
/// when an open fetched one, and creating the file fetched none: the append fails with EIO.
// contract: C12
#[test]
fn an_offline_append_to_a_file_this_client_saved_is_committed() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    save(&lab.mount_path("b").join("log"), b"first\n")?;
    append_offline(&lab, "log", b"second\n", b"first\nsecond\n")
}

/// Repro: b reads `log`, then lists a directory of 4,200 entries, then loses its authority link
/// and appends to `log`. Listing more than 4,096 entries makes b drop every entry it fetched over
/// a second ago, `log`'s among them, so the lookup for the append fails with EIO although b holds
/// `log`'s current bytes.
// contract: C12
#[test]
fn an_offline_append_after_listing_a_big_directory_is_committed() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join("log"), b"first\n")?;
    let parent = mkdir(&lab, "big")?;
    for index in 0..MANY {
        let path = SpacePath::try_from(format!("big/{index}"))?;
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
    assert_bytes(
        &fs::read(lab.mount_path("b").join("log"))?,
        b"first\n",
        "b's read",
    );
    thread::sleep(VISIBLE_AFTER);
    assert_eq!(fs::read_dir(lab.mount_path("b").join("big"))?.count(), MANY);
    append_offline(&lab, "log", b"second\n", b"first\nsecond\n")
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

//! Round 12: an offline append to a file its own client saved and then renamed (C12).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::thread;

use anyhow::Result;
use herdfs_lab::Links;

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

/// a saves `f` and, after T_vis, stats it, so a caches its committed entry. With `rename`, a then
/// renames `f` to `g`, as `mv` does. After T_vis, so the kernel asks again, a loses its authority
/// link and appends a line to the file under its current name. Returns what the append returned
/// and, once a has reconnected and committed, what b reads there.
fn append_offline_after(rename: bool) -> Result<Option<(io::Result<()>, Vec<u8>)>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let path = lab.mount_path("a").join("f");
    save(&path, b"first\n")?;
    thread::sleep(VISIBLE_AFTER);
    fs::metadata(&path)?;
    let name = if rename { "g" } else { "f" };
    if rename {
        fs::rename(&path, lab.mount_path("a").join(name))?;
    }
    thread::sleep(VISIBLE_AFTER);
    lab.cut("a", Links::Authority)?;
    let appended = OpenOptions::new()
        .append(true)
        .open(lab.mount_path("a").join(name))
        .and_then(|mut file| file.write_all(b"second\n"));
    lab.restore("a", Links::Authority)?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    Ok(Some((appended, fs::read(lab.mount_path("b").join(name))?)))
}

/// Repro: [`append_offline_after`] with the rename. The rename drops the entries a cached at both
/// names (fs/namespace.rs `rename`, fs/paths.rs `stale`), and offline a answers a lookup only from
/// an entry cached at that path (fs/paths.rs `held`), so the append's lookup of `g` fails with EIO,
/// although a knows the inode is at `g` and holds every byte of its current version. "Content
/// writes made offline to existing files are journaled and committed on reconnect ... when ...
/// this client holds the file's current bytes" (C12). The control is
/// [`an_offline_append_to_a_file_this_client_saved_lands`], without the rename, where the append
/// is committed on reconnect.
// contract: C12
#[test]
#[ignore = "ROUND12: offline, an append to a file this client saved and renamed fails with EIO"]
fn an_offline_append_to_a_file_this_client_renamed_lands() -> Result<()> {
    let Some((appended, read)) = append_offline_after(true)? else {
        return Ok(());
    };
    assert!(appended.is_ok(), "a's offline append: {appended:?}");
    assert_bytes(&read, b"first\nsecond\n", "b's read of g");
    Ok(())
}

// contract: C12
#[test]
fn an_offline_append_to_a_file_this_client_saved_lands() -> Result<()> {
    let Some((appended, read)) = append_offline_after(false)? else {
        return Ok(());
    };
    assert!(appended.is_ok(), "a's offline append: {appended:?}");
    assert_bytes(&read, b"first\nsecond\n", "b's read of f");
    Ok(())
}

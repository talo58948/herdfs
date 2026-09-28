//! Round 13: a lookup on one client, by a name another client moved an entry away from, after a
//! directory listing that names the entry at its new path.

use std::fs;
use std::thread;

use anyhow::Result;

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

/// a saves `f` and `f.new`; b lists the mount's root, as `ls` does, so b knows both. a moves `f`
/// to `f.old` and `f.new` to `f`, as a save that keeps a backup does. After T_vis, with `list`, b
/// lists the root again; then b reads `f`. Returns what b reads.
fn read_after_a_swap(list: bool) -> Result<Option<Vec<u8>>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    save(&a.join("f"), b"old")?;
    save(&a.join("f.new"), b"new")?;
    thread::sleep(VISIBLE_AFTER);
    fs::read_dir(b)?.try_for_each(|listed| listed.map(drop))?;

    fs::rename(a.join("f"), a.join("f.old"))?;
    fs::rename(a.join("f.new"), a.join("f"))?;
    thread::sleep(VISIBLE_AFTER);
    if list {
        fs::read_dir(b)?.try_for_each(|listed| listed.map(drop))?;
    }
    Ok(Some(fs::read(b.join("f"))?))
}

/// Repro: [`read_after_a_swap`] with the second listing. A listing takes each entry's attributes,
/// with its new path, into the entry's inode as fresh (fs/inodes.rs `observe`), but moves no
/// binding of a path it names (`bind` is skipped for a known entry), so `f` stays bound to the
/// entry now at `f.old`, and the lookup of `f` answers from that inode's fresh attributes
/// (fs/inodes.rs `at`, fs/ask.rs `entry`) without asking. b reads the backup at `f`, well after
/// T_vis. The control is [`a_file_saved_with_a_backup_reads_new_on_another_client`], without the
/// second listing, where the lookup asks the authority and b reads the new file.
// contract: C7, C2 (other clients follow a rename within T_vis; an open that starts T_vis after
// the save sees it)
#[test]
#[ignore = "ROUND13: after a listing, the name another client moved a file from still finds it"]
fn a_file_saved_with_a_backup_reads_new_after_a_listing() -> Result<()> {
    let Some(read) = read_after_a_swap(true)? else {
        return Ok(());
    };
    assert_bytes(&read, b"new", "b's read of f after listing the directory");
    Ok(())
}

// contract: C7, C2
#[test]
fn a_file_saved_with_a_backup_reads_new_on_another_client() -> Result<()> {
    let Some(read) = read_after_a_swap(false)? else {
        return Ok(());
    };
    assert_bytes(&read, b"new", "b's read of f");
    Ok(())
}

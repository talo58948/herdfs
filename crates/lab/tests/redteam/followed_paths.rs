//! Round 3: operations on entries this client holds, after another client moves them (C7).

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};

use crate::{VISIBLE_AFTER, two_clients};

/// When b makes `sub1` and stats `d`, after its listing: the stat within T_vis of it, so the
/// listing's cached entries answer it.
const SUB1_AT: Duration = Duration::from_millis(700);
const STAT_AT: Duration = Duration::from_millis(850);
/// When b makes `sub2`, after the rename: past T_vis.
const SUB2_AFTER_RENAME: Duration = Duration::from_millis(1_100);

fn sleep_until(deadline: Instant) {
    thread::sleep(deadline.saturating_duration_since(Instant::now()));
}

/// Repro: b holds directory `d` open, as a shell's working directory, for more than T_vis; b
/// lists the root; a at once renames `d` to `e` and makes a new, empty `d`; 0.7 s after the
/// listing b makes `sub1` in its directory, which asks the authority where the entry is now
/// (`Locate`) and lands in `e`; 0.15 s later b stats `d`, which the listing's cached entry still
/// answers with the moved directory, and the client takes `d` for its path again but keeps the
/// time it confirmed `e` (fs/mod.rs `looked_up`); 1.1 s after the rename b makes `sub2` in its
/// directory, and it lands in the new `d`.
// contract: C7
#[test]
fn operations_in_a_moved_directory_follow_it_after_t_vis() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    fs::create_dir(a.join("d"))?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join("d"))?;
    let inside = |name: &str| PathBuf::from(format!("/proc/self/fd/{}/{name}", held.as_raw_fd()));
    thread::sleep(VISIBLE_AFTER);

    fs::read_dir(b)?.count();
    let listed = Instant::now();
    fs::rename(a.join("d"), a.join("e"))?;
    let renamed = Instant::now();
    fs::create_dir(a.join("d"))?;
    sleep_until(listed + SUB1_AT);
    fs::create_dir(inside("sub1"))?;
    sleep_until(listed + STAT_AT);
    fs::metadata(b.join("d"))?;
    let stated = listed.elapsed();
    sleep_until(renamed + SUB2_AFTER_RENAME);
    fs::create_dir(inside("sub2"))?;
    ensure!(
        stated < VISIBLE_AFTER - Duration::from_millis(200),
        "the stat came {stated:?} after the listing, too late to be answered from it"
    );
    thread::sleep(VISIBLE_AFTER);
    ensure!(a.join("e/sub1").is_dir(), "sub1, made before the stat, is in e");
    let landed = ["d", "e"].map(|dir| a.join(dir).join("sub2").is_dir());
    assert_eq!(
        landed,
        [false, true],
        "whether sub2, made in the held directory {SUB2_AFTER_RENAME:?} after it moved from d to e, \
         is in d and in e"
    );
    Ok(())
}

/// Repro: b opens `t` and keeps the descriptor; a renames `t` to `u`; b saves a new `t` the way
/// editors do, a temporary file renamed over the name; T_vis later b fstats its descriptor. b's
/// rename drops every inode whose last known path is `t` (fs/namespace.rs `rename`), the moved
/// file's among them, and an inode without a path cannot be located (fs/mod.rs `located`), so the
/// fstat of an open file that was never unlinked fails with ENOENT, as does every call that needs
/// its path, until something looks it up by its new name. Without b's rename the fstat finds it.
// contract: C7
#[test]
fn a_file_held_open_is_followed_after_its_old_name_is_reused() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    crate::save(&a.join("t"), b"moved")?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join("t"))?;
    fs::rename(a.join("t"), a.join("u"))?;
    thread::sleep(VISIBLE_AFTER);
    crate::save(&b.join("t.tmp"), b"new")?;
    fs::rename(b.join("t.tmp"), b.join("t"))?;
    thread::sleep(VISIBLE_AFTER);
    let size = held.metadata().map(|metadata| metadata.len());
    assert_eq!(
        size.map_err(|error| error.to_string()),
        Ok(5),
        "fstat of the descriptor b holds on the file a moved from t to u"
    );
    Ok(())
}

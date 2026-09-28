//! Product bugs pjdfstest found, each reduced to a test: ignored while the bug stands, run once
//! it is fixed.
//! contract: none; these are POSIX behaviors, and the errors docs/architecture.md promises for
//! features out of v0.

use std::fs::{self, File};
use std::os::unix::net::UnixListener;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;

use crate::lab;

/// The pjdfstest tests each bug fails, keyed by the test below that reproduces it.
pub const BUGS: &[(&str, &[&str])] = &[];

/// Past the millisecond resolution of stored times.
const TICK: Duration = Duration::from_millis(10);
/// T_vis plus a margin, after which the kernel and the client ask the authority again.
const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);

#[test]
fn an_overlong_name_fails_with_enametoolong() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    let error = fs::create_dir(lab.mount_path("a").join("n".repeat(256))).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENAMETOOLONG), "{error}");
    Ok(())
}

#[test]
fn adding_an_entry_changes_the_directorys_mtime() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    let dir = lab.mount_path("a").join("dir");
    fs::create_dir(&dir)?;
    let before = fs::metadata(&dir)?.modified()?;
    thread::sleep(TICK);
    fs::create_dir(dir.join("child"))?;
    assert!(fs::metadata(&dir)?.modified()? > before);
    Ok(())
}

#[test]
fn creating_a_special_file_fails_cleanly() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    let error = UnixListener::bind(lab.mount_path("a").join("socket")).unwrap_err();
    assert!(
        matches!(error.raw_os_error(), Some(libc::EPERM | libc::ENOTSUP)),
        "{error}"
    );
    Ok(())
}

#[test]
fn an_mtime_set_after_a_write_survives_its_commit() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    let path = lab.mount_path("a").join("copied");
    // Large enough that the mtime is set before the commit lands, as `cp -p` and `rsync -t` do.
    fs::write(&path, vec![1; 4 << 20])?;
    let mtime = UNIX_EPOCH + Duration::from_secs(1_900_000_000);
    File::open(&path)?.set_modified(mtime)?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    assert_eq!(fs::metadata(&path)?.modified()?, mtime);
    Ok(())
}

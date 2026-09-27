//! Product bugs pjdfstest found, each reduced to a test: ignored while the bug stands, run once
//! it is fixed.
//! contract: none; these are POSIX behaviors, and the errors docs/architecture.md promises for
//! features out of v0.

use std::fs::{self, File, FileTimes, Permissions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;

use crate::lab;

/// The pjdfstest tests each bug fails, keyed by the test below that reproduces it.
pub const BUGS: &[(&str, &[&str])] = &[
    (
        "a_set_atime_sticks",
        &[
            "utimensat::changes_timestamps::dir",
            "utimensat::changes_timestamps::regular",
            "utimensat::follow_symlink",
            "utimensat::subsecond",
            "utimensat::utime_omit",
            "utimensat::y2038",
        ],
    ),
    (
        "adding_an_entry_changes_the_directorys_mtime",
        &[
            "mkdir::changed_time_fields_success",
            "open::changed_time_fields_success",
            "rmdir::changed_time_parent_success",
            "symlink::changed_parent_time_success",
            "unlink::update_mtime_ctime_success_folder::regular",
            "unlink::update_mtime_ctime_success_folder::symlink",
        ],
    ),
    ("chmod_changes_ctime", &["chmod::update_ctime::dir"]),
    (
        "an_unlinked_open_file_stays_usable",
        &["unlink::open_file_not_freed"],
    ),
];

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
#[ignore = "BUG: atime is not stored; stat reports mtime as atime"]
fn a_set_atime_sticks() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    let dir = lab.mount_path("a").join("dir");
    fs::create_dir(&dir)?;
    let [atime, mtime] = [1_900_000_000, 1_950_000_000].map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
    File::open(&dir)?.set_times(FileTimes::new().set_accessed(atime).set_modified(mtime))?;
    let metadata = fs::metadata(&dir)?;
    assert_eq!((metadata.accessed()?, metadata.modified()?), (atime, mtime));
    Ok(())
}

#[test]
#[ignore = "BUG: a directory's mtime and ctime stay put when an entry is added or removed"]
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
#[ignore = "BUG: ctime is always mtime, so chmod leaves it unchanged"]
fn chmod_changes_ctime() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    let dir = lab.mount_path("a").join("dir");
    fs::create_dir(&dir)?;
    let ctime = || fs::metadata(&dir).map(|metadata| (metadata.ctime(), metadata.ctime_nsec()));
    let before = ctime()?;
    thread::sleep(TICK);
    fs::set_permissions(&dir, Permissions::from_mode(0o700))?;
    assert!(ctime()? > before);
    Ok(())
}

#[test]
#[ignore = "BUG: fstat on an open file fails with ENOENT once the file is unlinked"]
fn an_unlinked_open_file_stays_usable() -> Result<()> {
    let Some(lab) = lab()? else { return Ok(()) };
    let path = lab.mount_path("a").join("scratch");
    let mut file = File::options()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)?;
    fs::remove_file(&path)?;
    file.write_all(b"scratch")?;
    let metadata = file.metadata()?;
    assert_eq!((metadata.len(), metadata.nlink()), (7, 0));
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

//! Writes while a client's authority link is cut (C12).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::thread;

use anyhow::Result;
use herdfs_lab::Links;

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

/// b lists a directory holding a file, loses its authority link, and appends to the file. b
/// holds none of the file's bytes, so C12 does not cover the append: it fails with a clean EIO
/// and leaves nothing to commit.
// contract: C12
#[test]
fn an_offline_append_to_a_file_this_client_never_read_fails_cleanly() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join("log"), b"first\n")?;
    thread::sleep(VISIBLE_AFTER);
    let log = lab.mount_path("b").join("log");
    assert_eq!(fs::read_dir(lab.mount_path("b"))?.count(), 1);
    assert_eq!(fs::metadata(&log)?.len(), 6);

    lab.cut("b", Links::Authority)?;
    let append = OpenOptions::new()
        .append(true)
        .open(&log)
        .and_then(|mut file| file.write_all(b"second\n"));
    lab.restore("b", Links::Authority)?;
    assert_eq!(append.map_err(|error| error.raw_os_error()), Err(Some(libc::EIO)));
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(&fs::read(lab.mount_path("a").join("log"))?, b"first\n", "a's log");
    Ok(())
}

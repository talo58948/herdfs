//! Writes while a client's authority link is cut (C12).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::thread;

use anyhow::Result;
use herdfs_lab::Links;

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

/// Repro: b lists a directory holding a file, loses its authority link, and appends to the file.
/// The open needs the head's bytes to start a journal copy, and b never opened the file, so it
/// fails with EIO instead of journaling the append.
// contract: C12
#[test]
#[ignore = "REDTEAM: an offline append to an existing file b never opened fails with EIO"]
fn an_offline_append_to_an_existing_file_commits_on_reconnect() -> Result<()> {
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
    assert!(append.is_ok(), "appending offline: {append:?}");
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(
        &fs::read(lab.mount_path("a").join("log"))?,
        b"first\nsecond\n",
        "a's log",
    );
    Ok(())
}

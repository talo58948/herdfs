//! Round 47: fsync through one handle after another handle changed the same file.

use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::thread;

use anyhow::Result;

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

const TRIALS: usize = 3;

fn synced_bytes(name: &str, sibling: bool) -> Result<Option<Vec<u8>>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let path = a.join(name);
    save(&path, b"old bytes")?;
    let writer = File::options().write(true).open(&path)?;
    writer.write_all_at(b"new bytes", 0)?;
    if sibling {
        File::open(&path)?.sync_all()?;
    } else {
        writer.sync_all()?;
    }
    thread::sleep(VISIBLE_AFTER);
    Ok(Some(fs::read(b.join(name))?))
}

/// An fsync flushes the file, including writes made through another descriptor. Fsync on the
/// writing descriptor is the control; three sibling-descriptor fsyncs must publish the same bytes.
// contract: C2 (fsync waits for the commit)
#[test]
fn fsync_through_a_sibling_descriptor_commits_the_files_writes() -> Result<()> {
    let Some(control) = synced_bytes("sibling-fsync-control", false)? else {
        return Ok(());
    };
    assert_bytes(&control, b"new bytes", "same-descriptor fsync control");

    let mut failures = 0;
    for trial in 0..TRIALS {
        let name = format!("sibling-fsync-{trial}");
        let Some(bytes) = synced_bytes(&name, true)? else {
            return Ok(());
        };
        eprintln!("trial {trial}: remote bytes={bytes:?}");
        failures += usize::from(bytes != b"new bytes");
    }
    assert_eq!(
        failures, 0,
        "sibling fsyncs that returned before the commit, of {TRIALS}"
    );
    Ok(())
}

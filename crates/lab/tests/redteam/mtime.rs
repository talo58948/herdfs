//! Modification times set through an open file.

use std::fs::{self, File};
use std::process::Command;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use herdfs_lab::run;

use crate::{VISIBLE_AFTER, two_clients};

/// Repro: `cp -p` into the mount. cp sets the mtime with futimens on the destination before it
/// closes it; the setattr lands while the write is not yet sealed, and the commit that close
/// starts then stamps the authority's clock over it.
// contract: none (docs/architecture.md: mtime is stored and reported; only atime and ctime are out of v0)
#[test]
fn cp_p_keeps_the_source_mtime() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let source = tempfile::NamedTempFile::new()?;
    fs::write(source.path(), b"kept")?;
    let mtime = UNIX_EPOCH + Duration::from_secs(1_900_000_000);
    File::options()
        .write(true)
        .open(source.path())?
        .set_modified(mtime)?;
    let copy = lab.mount_path("a").join("copy");
    run(Command::new("cp").arg("-p").arg(source.path()).arg(&copy))?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    assert_eq!(fs::metadata(&copy)?.modified()?, mtime, "a's copy");
    assert_eq!(
        fs::metadata(lab.mount_path("b").join("copy"))?.modified()?,
        mtime,
        "b's copy"
    );
    Ok(())
}

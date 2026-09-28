//! Round 7: modification times before 1970, which a local disk keeps.

use std::fs::{self, File};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::{VISIBLE_AFTER, two_clients};

/// 1969-07-20 20:17:00 UTC, in seconds before the epoch.
const BEFORE_EPOCH: Duration = Duration::from_secs(14_182_980);

/// Repro: a writes `f` and sets its mtime to 1969-07-20 20:17 UTC, as `touch -d` does, or
/// `tar -x` of an archive holding such a time; a stats `f`, then b does once it has committed.
/// Both read 1970-01-01 00:00: setattr turns every time before the epoch into the epoch
/// (fs/fuse.rs `setattr` through lib.rs `unix_ms`), as do fs/mod.rs `attr` and
/// journal/working.rs `set_mtime` on the way back, though the protocol carries signed
/// milliseconds. A local directory keeps the time; make takes any two such files as equally old.
// contract: none (docs/architecture.md: mtime is stored and reported; only atime and ctime are out of v0)
#[test]
#[ignore = "REDTEAM7: a modification time before 1970 reads back as 1970-01-01"]
fn an_mtime_before_1970_is_kept() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let path = lab.mount_path("a").join("f");
    fs::write(&path, "landing\n")?;
    let landing = SystemTime::UNIX_EPOCH - BEFORE_EPOCH;
    File::options().write(true).open(&path)?.set_modified(landing)?;
    assert_eq!(fs::metadata(&path)?.modified()?, landing, "a's mtime of `f`");
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    let seen = fs::metadata(lab.mount_path("b").join("f"))?.modified()?;
    assert_eq!(seen, landing, "b's mtime of `f`");
    Ok(())
}

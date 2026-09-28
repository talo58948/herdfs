//! Round 5: make, which decides what to rebuild by comparing modification times.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use herdfs_lab::run;

use crate::{VISIBLE_AFTER, two_clients};

/// `out` is `in` followed by 64 MiB, so that it takes a while to commit.
const MAKEFILE: &str = "out: in\n\tcat in > out\n\thead -c 67108864 /dev/zero | tr '\\0' x >> out\n";
/// Between the build and the edit: past the clock tick of a local file system's timestamps, so
/// that a local directory tells the two writes apart, and far inside the commit of `out`.
const EDIT_AFTER: Duration = Duration::from_millis(50);

/// Repro: on a, `make` builds `out`, 64 MiB, from `in`; 50 ms later `in` is edited, as a script
/// that patches a source between two builds does; once both have committed, `make -q`
/// says `out` is up to date. A commit stamps the authority's clock at commit time as the file's
/// mtime (versions.rs `commit`, `mtime_ms.unwrap_or(self.now_ms)`), after the upload: `out`,
/// closed first, commits seconds after `in`, edited after it, and ends up the newer. make then
/// never rebuilds `out`, which stays built from the old `in`. Before the commits, a reports each
/// file's time of last write and make would rebuild; on a local directory it does.
// contract: none (docs/architecture.md: mtime is stored and reported; only atime and ctime are out of v0)
#[test]
#[ignore = "REDTEAM5: commit time as mtime makes an older write look newer, so make skips a rebuild"]
fn make_rebuilds_a_target_whose_source_changed_after_it() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let dir = lab.mount_path("a");
    fs::write(dir.join("Makefile"), MAKEFILE)?;
    fs::write(dir.join("in"), "v1\n")?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    run(Command::new("make").arg("-s").current_dir(dir))?;
    thread::sleep(EDIT_AFTER);
    fs::write(dir.join("in"), "v2\n")?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    let up_to_date = Command::new("make")
        .arg("-q")
        .current_dir(dir)
        .status()?
        .success();
    let mtime = |name: &str| fs::metadata(dir.join(name)).and_then(|meta| meta.modified());
    assert!(
        !up_to_date,
        "make takes `out` as up to date: in {:?}, out {:?}",
        mtime("in")?,
        mtime("out")?
    );
    Ok(())
}

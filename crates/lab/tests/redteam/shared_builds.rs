//! Round 6: make on one client, from a source saved on another.

use std::fs;
use std::process::Command;
use std::thread;

use anyhow::Result;
use herdfs_lab::run;

use crate::{VISIBLE_AFTER, two_clients};

const MAKEFILE: &str = "out: in\n\tcp in out\n";

/// Repro: a saves a new `in` without fsync, as an editor does, and b at once runs `make`, which
/// builds `out` from `in`. b still reads the old `in`: a's commit comes a quiet period after its
/// close (journal.rs `QUIET`), which C2 allows. Once a's commit has landed and T_vis has passed,
/// `make -q` on b says `out` is up to date, and `out` keeps the old bytes for good. The commit
/// stamps `in` with the time of a's write (journal.rs `seal`, round 5), which is before b built
/// `out`, so `in` looks older than an `out` built from its previous bytes. On a local directory,
/// or if a fsyncs `in` before b builds, b's `out` gets the new bytes.
// contract: none (docs/architecture.md: mtime is stored and reported; only atime and ctime are out of v0)
#[test]
#[ignore = "REDTEAM6: a source saved on one client looks older than the target another client then builds from its old bytes"]
fn make_on_another_client_rebuilds_after_a_source_commits() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    fs::write(a.join("Makefile"), MAKEFILE)?;
    fs::write(a.join("in"), "v1\n")?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    fs::read(b.join("in"))?;

    fs::write(a.join("in"), "v2\n")?;
    run(Command::new("make").arg("-s").current_dir(b))?;
    lab.wait_committed("a")?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    let up_to_date = Command::new("make").arg("-q").current_dir(b).status()?.success();
    let out = fs::read_to_string(b.join("out"))?;
    assert!(
        !up_to_date || out == "v2\n",
        "make on b takes `out` as up to date, built from {out:?} while `in` holds {:?}",
        fs::read_to_string(b.join("in"))?
    );
    Ok(())
}

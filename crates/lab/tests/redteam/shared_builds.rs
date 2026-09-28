//! Round 6: make on one client, from a source saved on another.

use std::fs::{self, File};
use std::io::Write;
use std::process::Command;
use std::thread;

use anyhow::Result;
use herdfs_lab::run;

use crate::{VISIBLE_AFTER, two_clients};

const MAKEFILE: &str = "out: in\n\tcp in out\n";

/// a saves a new `in` and fsyncs it, and b, T_vis later, runs `make`, which rebuilds `out` from
/// `in`'s new bytes: a tool on another client that compares times runs after the writer's fsync
/// (C2).
///
/// Not supported: a saves `in` without fsync, as an editor does, and b at once runs `make`. b
/// still reads the old `in`, since a's commit comes a quiet period after its close (journal.rs
/// `QUIET`), which C2 allows. The commit stamps `in` with the time of a's write, before b built
/// `out`, so once it lands `make -q` on b takes `out`, built from the old bytes, as up to date.
/// On a local directory b's `out` gets the new bytes.
// contract: C2
#[test]
fn make_on_another_client_rebuilds_from_a_source_saved_with_fsync() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    fs::write(a.join("Makefile"), MAKEFILE)?;
    fs::write(a.join("in"), "v1\n")?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    run(Command::new("make").arg("-s").current_dir(b))?;
    assert_eq!(fs::read_to_string(b.join("out"))?, "v1\n", "b's first build");

    let mut source = File::create(a.join("in"))?;
    source.write_all(b"v2\n")?;
    source.sync_all()?;
    drop(source);
    thread::sleep(VISIBLE_AFTER);
    run(Command::new("make").arg("-s").current_dir(b))?;
    assert_eq!(
        fs::read_to_string(b.join("out"))?,
        "v2\n",
        "b's build after a saved `in` with fsync"
    );
    Ok(())
}

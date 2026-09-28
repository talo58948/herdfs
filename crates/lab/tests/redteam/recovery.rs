//! Round 3: restarting a client that a crash stopped while it settled a commit.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Result, ensure};
use herdfs_lab::Links;

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

/// The commit records in a client's journal, one per entry with a closed write to commit.
fn records(journal: &Path) -> Result<Vec<PathBuf>> {
    let mut records = Vec::new();
    for item in fs::read_dir(journal)? {
        let path = item?.path();
        if path.extension().is_some_and(|extension| extension == "json") {
            records.push(path);
        }
    }
    Ok(records)
}

/// Repro: b overwrites `f` while its authority link is cut, and the test keeps a copy of the
/// record journaling that write; b reconnects and commits `f`, and the committer settles it by
/// removing the sealed copy and then the record (commit.rs `Journal::finish`); b goes offline
/// again, overwrites `g`, and is SIGKILLed; the test puts `f`'s record back, which is what a
/// SIGKILL between those two removes leaves. On restart the journal cannot open the sealed copy
/// the record names, so the mount refuses to start (journal.rs `recover`), and `g`'s closed
/// write never commits.
// contract: C4a
#[test]
#[ignore = "REDTEAM3: a crash between the two removes that settle a commit keeps the client from mounting"]
fn a_client_killed_while_settling_a_commit_restarts_and_commits_its_other_writes() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a").to_owned(), lab.mount_path("b").to_owned());
    save(&a.join("f"), b"f0")?;
    save(&a.join("g"), b"g0")?;
    thread::sleep(VISIBLE_AFTER);
    // Looked up online, so that b can open them offline.
    fs::metadata(b.join("f"))?;
    fs::metadata(b.join("g"))?;
    let journal = lab.client_dir("b").join("journal");

    lab.cut("b", Links::Authority)?;
    fs::write(b.join("f"), b"f1")?;
    let Ok([record]) = <[PathBuf; 1]>::try_from(records(&journal)?) else {
        anyhow::bail!("b's journal holds no single record for f");
    };
    let kept = fs::read(&record)?;
    lab.restore("b", Links::Authority)?;
    lab.wait_committed("b")?;
    ensure!(!record.exists(), "f's commit has settled");

    lab.cut("b", Links::Authority)?;
    fs::write(b.join("g"), b"g1")?;
    lab.kill_client("b")?;
    fs::write(&record, kept)?;
    lab.restore("b", Links::Authority)?;
    let restarted = lab.restart_client("b");
    assert!(restarted.is_ok(), "b's restart: {restarted:?}");
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(&fs::read(a.join("g"))?, b"g1", "a's read of g");
    Ok(())
}

//! Round 19: the authority dies while its write of an upload to the store is stalled, after the
//! permission check and before the record (the upload order of round 17).

use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, StoppedProcess, process_with_arg};

use crate::{VISIBLE_AFTER, random_bytes, save, two_clients};

/// Several chunks, so the upload is under way when the store stops answering.
const LEN: usize = 8 << 20;
/// Long enough for the committer to reach the stalled store write.
const STALL: Duration = Duration::from_secs(3);
const READ_WITHIN: Duration = Duration::from_secs(30);

/// Whether `client` reads exactly `expected` at `name` within [`READ_WITHIN`].
fn reads(lab: &Lab, client: &str, name: &str, expected: &[u8]) -> bool {
    let (done, got) = mpsc::channel();
    let path = lab.mount_path(client).join(name);
    // Detached, so that a read the client never answers fails the test instead of hanging it.
    thread::spawn(move || done.send(fs::read(path)));
    got.recv_timeout(READ_WITHIN)
        .is_ok_and(|got| got.is_ok_and(|got| got == expected))
}

/// What a sees and b reads after the crash, per file.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// b reads the file a saved with fsync before the stall (C4b).
    synced: bool,
    /// b reads the file a closed during the stall (C4a).
    closed: bool,
    /// a reads the file it closed during the stall (C1).
    own: bool,
}

/// a saves `synced` with fsync; the test stops MinIO; a writes `closed` and closes it, so its
/// commit's uploads stall in the authority's store write; after [`STALL`], with `crash`, the
/// test SIGKILLs the authority. With `restart_first` it restarts the authority while the store
/// is still stopped and resumes the store [`STALL`] later; else it resumes the store first.
/// Once a has nothing left to commit, reports what b and a read.
fn crash_during_a_stalled_store_write(crash: bool, restart_first: bool) -> Result<Option<Outcome>> {
    let Some(mut lab) = two_clients()? else {
        return Ok(None);
    };
    let (synced, closed) = (random_bytes(LEN), random_bytes(LEN));
    save(&lab.mount_path("a").join("synced"), &synced)?;
    let pid = process_with_arg(lab.store_dir())?;
    let stopped = StoppedProcess::new(pid)?;
    fs::write(lab.mount_path("a").join("closed"), &closed)?;
    thread::sleep(STALL);
    let pending = lab.stats("a")?["pending_commits"].as_u64();
    ensure!(pending > Some(0), "the closed file's commit waits on the store");
    if crash {
        lab.kill_authority();
        if restart_first {
            lab.restart_authority()?;
            thread::sleep(STALL);
            drop(stopped);
        } else {
            drop(stopped);
            lab.restart_authority()?;
        }
    } else {
        drop(stopped);
    }
    lab.wait_committed("a").context("a commits the closed file")?;
    thread::sleep(VISIBLE_AFTER);
    Ok(Some(Outcome {
        synced: reads(&lab, "b", "synced", &synced),
        closed: reads(&lab, "b", "closed", &closed),
        own: reads(&lab, "a", "closed", &closed),
    }))
}

const WHOLE: Outcome = Outcome {
    synced: true,
    closed: true,
    own: true,
};

/// The authority dies inside a stalled store write, and restarts once the store answers again.
/// The control is [`a_stalled_store_write_without_a_crash_commits`].
// contract: C4a, C4b, C1
#[test]
fn a_crash_during_a_stalled_store_write_loses_nothing() -> Result<()> {
    if let Some(outcome) = crash_during_a_stalled_store_write(true, false)? {
        assert_eq!(outcome, WHOLE);
    }
    Ok(())
}

/// As above, but the authority restarts while the store is still stopped, so its first uploads
/// after the restart stall too.
// contract: C4a, C4b, C1
#[test]
fn a_crash_during_a_stalled_store_write_and_a_restart_before_it_answers_loses_nothing() -> Result<()> {
    if let Some(outcome) = crash_during_a_stalled_store_write(true, true)? {
        assert_eq!(outcome, WHOLE);
    }
    Ok(())
}

/// Control: the store stalls and resumes, and the authority stays up.
// contract: C4a, C4b, C1
#[test]
fn a_stalled_store_write_without_a_crash_commits() -> Result<()> {
    if let Some(outcome) = crash_during_a_stalled_store_write(false, false)? {
        assert_eq!(outcome, WHOLE);
    }
    Ok(())
}

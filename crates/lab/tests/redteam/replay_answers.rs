//! Round 16 probe: a journal retry whose commit landed but whose reply was lost, answered after a
//! newer commit and a purge removed the retried version: with the head while the author may read,
//! else with `Done`.

use std::fs::{self, File};
use std::thread;
use std::time::Duration;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, Lab, LabConfig, Links, SPACE, Strategy};

use crate::{SLOW_ANSWERS, VISIBLE_AFTER, save, set_alice, wait_for_rewrite};

const TRIALS: usize = 3;
/// Alice rewrites a file and holds it open; her commit lands but its reply is cut. The admin then
/// commits an empty head and purges the rest, removing alice's version; with `keep_read` alice
/// keeps read on the root, else she loses all access. Her link comes back and the journal
/// retries. Returns, per trial, whether `fsync` succeeded and what alice reads T_vis later (with
/// write access given back first).
fn retry_after_purge(keep_read: bool) -> Result<Option<Vec<(bool, String)>>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default().client("a", "alice", &[], SLOW_ANSWERS, Strategy::Stream);
    let lab = Lab::start(config)?;
    let mut outcomes = Vec::new();
    for trial in 0..TRIALS {
        let name = format!("replay-{trial}");
        let path = lab.mount_path("a").join(&name);
        save(&path, b"first")?;
        let first = lab.head(ADMIN, &name)?;
        fs::write(&path, b"rewrite")?;
        let held = File::open(&path)?;
        wait_for_rewrite(&lab, &name, &first)?;
        lab.cut("a", Links::Authority)?;
        thread::sleep(Duration::from_millis(600));
        ensure!(
            lab.stats("a")?["pending_commits"] == 1,
            "trial {trial}: settled before the cut"
        );

        let commit = Request::Commit {
            space: SPACE.into(),
            entry: first.entry,
            request_id: format!("admin-{trial}"),
            base: None,
            guarded: false,
            chunks: Vec::new(),
            mtime_ms: None,
        };
        lab.rpc(ADMIN, &commit)?;
        let at = SpacePath::try_from(name.as_str())?;
        lab.rpc(
            ADMIN,
            &Request::Purge {
                space: SPACE.into(),
                path: at,
            },
        )?;
        set_alice(&lab, keep_read.then_some(Perm::Read))?;
        lab.restore("a", Links::Authority)?;
        lab.wait_committed("a")?;
        let synced = held.sync_all();
        drop(held);

        set_alice(&lab, Some(Perm::Write))?;
        thread::sleep(VISIBLE_AFTER);
        let read = fs::read(&path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        outcomes.push((synced.is_ok(), read.unwrap_or_else(|err| err.to_string())));
    }
    Ok(Some(outcomes))
}

/// Answered with the head, alice's `fsync` succeeds and she then reads the admin's empty head.
// contract: C4a, C2, S8
#[test]
fn a_retry_answered_with_the_head_settles_on_it() -> Result<()> {
    let Some(outcomes) = retry_after_purge(true)? else {
        return Ok(());
    };
    assert_eq!(outcomes, vec![(true, String::new()); TRIALS]);
    Ok(())
}

/// Answered with `Done`, alice's `fsync` succeeds; given access back, she reads the admin's head.
// contract: C4a, C2, S8
#[test]
fn a_retry_answered_done_settles_and_reads_the_head_later() -> Result<()> {
    let Some(outcomes) = retry_after_purge(false)? else {
        return Ok(());
    };
    assert_eq!(outcomes, vec![(true, String::new()); TRIALS]);
    Ok(())
}

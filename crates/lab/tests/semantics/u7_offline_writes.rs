//! U7: a client writes while cut off from the authority, then reconnects.

use std::fs::{self, File};
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy};

use crate::support::{self, VISIBLE_AFTER, counter, history, wait_until};

const FILES: usize = 200;
/// Long enough for every file's first commit attempt to fail, so reconnecting means retrying.
const OFFLINE: Duration = Duration::from_secs(3);
/// The committer's backoff after `OFFLINE`, plus time to commit every file.
const DRAIN: Duration = Duration::from_secs(30);

fn two_clients() -> Result<Option<Lab>> {
    support::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )
}

#[test]
#[ignore = "BUG: create and open-for-write call the authority first, so offline writes fail with EIO"]
fn files_written_offline_commit_exactly_once_on_reconnect() -> Result<()> {
    // contract: C12, C5
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    lab.cut("a", Links::Authority)?;
    for n in 0..FILES {
        fs::write(lab.mount_path("a").join(name(n)), body(n))?;
    }
    reconnect_and_check(&lab)
}

#[test]
fn writes_closed_offline_commit_exactly_once_on_reconnect() -> Result<()> {
    // contract: C12, C5
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let files = (0..FILES)
        .map(|n| File::create(lab.mount_path("a").join(name(n))))
        .collect::<io::Result<Vec<_>>>()?;
    lab.cut("a", Links::Authority)?;
    for (n, mut file) in files.into_iter().enumerate() {
        file.write_all(body(n).as_bytes())?;
    }
    reconnect_and_check(&lab)
}

/// Keeps A offline a while, reconnects it, and checks that B sees each file with exactly one
/// version: retries after the outage must not commit twice.
fn reconnect_and_check(lab: &Lab) -> Result<()> {
    thread::sleep(OFFLINE);
    lab.restore("a", Links::Authority)?;
    let drained = || Ok(counter(lab, "a", "pending_commits")? == 0);
    wait_until("A commits its journal", DRAIN, drained)?;
    thread::sleep(VISIBLE_AFTER);
    for n in 0..FILES {
        let name = name(n);
        assert_eq!(fs::read_to_string(lab.mount_path("b").join(&name))?, body(n));
        assert_eq!(history(lab, "alice", &name)?.len(), 1, "versions of {name}");
    }
    Ok(())
}

fn name(n: usize) -> String {
    format!("offline-{n:03}.txt")
}

fn body(n: usize) -> String {
    format!("written offline, file {n}\n")
}

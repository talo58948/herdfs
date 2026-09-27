//! U7: a client writes while cut off from the authority, then reconnects.

use std::fs::{self, File};
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy};

use crate::support::{self, VISIBLE_AFTER, counter, errno, history, wait_until};

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
fn existing_files_overwritten_offline_commit_exactly_once_on_reconnect() -> Result<()> {
    // contract: C12, C5
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let on_a = lab.mount_path("a");
    for n in 0..FILES {
        support::save(&on_a.join(name(n)), b"before the outage\n")?;
    }
    lab.cut("a", Links::Authority)?;
    for n in 0..FILES {
        fs::write(on_a.join(name(n)), body(n))?;
    }
    reconnect_and_check(&lab, 2)
}

#[test]
fn creating_a_file_offline_fails_and_leaves_nothing_behind() -> Result<()> {
    // contract: C12
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    lab.cut("a", Links::Authority)?;
    assert_eq!(
        errno(fs::write(lab.mount_path("a").join(name(0)), body(0))),
        Some(libc::EIO)
    );
    lab.restore("a", Links::Authority)?;
    thread::sleep(VISIBLE_AFTER);
    assert_eq!(counter(&lab, "a", "pending_commits")?, 0);
    assert_eq!(
        fs::read_dir(lab.client_dir("a").join("journal"))?.count(),
        1,
        "only rejected/"
    );
    assert_eq!(
        errno(fs::metadata(lab.mount_path("b").join(name(0)))),
        Some(libc::ENOENT)
    );
    Ok(())
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
    reconnect_and_check(&lab, 1)
}

/// Keeps A offline a while, reconnects it, and checks that B sees each file's new bytes and
/// `versions` versions of it: retries after the outage must not commit twice.
fn reconnect_and_check(lab: &Lab, versions: usize) -> Result<()> {
    thread::sleep(OFFLINE);
    lab.restore("a", Links::Authority)?;
    let drained = || Ok(counter(lab, "a", "pending_commits")? == 0);
    wait_until("A commits its journal", DRAIN, drained)?;
    thread::sleep(VISIBLE_AFTER);
    for n in 0..FILES {
        let name = name(n);
        assert_eq!(fs::read_to_string(lab.mount_path("b").join(&name))?, body(n));
        assert_eq!(
            history(lab, "alice", &name)?.len(),
            versions,
            "versions of {name}"
        );
    }
    Ok(())
}

fn name(n: usize) -> String {
    format!("offline-{n:03}.txt")
}

fn body(n: usize) -> String {
    format!("written offline, file {n}\n")
}

//! Round 37: repeated rebases and version switches through held entries.

use std::fs::{self, File};
use std::path::Path;
use std::thread;

use anyhow::{Result, ensure};
use herdfs_lab::Links;

use crate::{
    VISIBLE_AFTER, assert_bytes, entry_names, random_bytes, read_at, read_entry, readlink_entry, save,
    two_clients, write_entry,
};

const TRIALS: usize = 3;
const SWITCHES: usize = 3;

fn list(dir: &File) -> Result<()> {
    ensure!(
        entry_names(dir)?.len() == SWITCHES,
        "held directory has all children"
    );
    Ok(())
}

/// Moves a held directory successively into paths whose decoy subtrees b has already listed.
/// After each move, an offline lookup of a different child must use the rebased original entry.
fn rebase_over_decoys(trial: usize) -> Result<Option<()>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let origin = format!("origin-{trial}");
    fs::create_dir(a.join(&origin))?;
    for child in 0..SWITCHES {
        std::os::unix::fs::symlink(
            format!("original-{trial}-{child}"),
            a.join(&origin).join(format!("child-{child}")),
        )?;
    }
    for hop in 0..SWITCHES {
        let destination = format!("destination-{trial}-{hop}");
        fs::create_dir(a.join(&destination))?;
        for child in 0..SWITCHES {
            std::os::unix::fs::symlink(
                format!("decoy-{trial}-{hop}-{child}"),
                a.join(&destination).join(format!("child-{child}")),
            )?;
        }
    }
    thread::sleep(VISIBLE_AFTER);

    let held = File::open(b.join(&origin))?;
    list(&held)?;
    for hop in 0..SWITCHES {
        let decoy = File::open(b.join(format!("destination-{trial}-{hop}")))?;
        list(&decoy)?;
    }

    let mut current = origin;
    for hop in 0..SWITCHES {
        let destination = format!("destination-{trial}-{hop}");
        fs::rename(a.join(&destination), a.join(format!("parked-{trial}-{hop}")))?;
        fs::rename(a.join(&current), a.join(&destination))?;
        thread::sleep(VISIBLE_AFTER);
        held.metadata()?;

        lab.cut("b", Links::Authority)?;
        let child = hop;
        let read = readlink_entry(&held, &format!("child-{child}"));
        lab.restore("b", Links::Authority)?;
        let expected = format!("original-{trial}-{child}");
        ensure!(
            read? == Path::new(&expected),
            "child below repeatedly rebased parent"
        );
        current = destination;
    }
    Ok(Some(()))
}

/// A stationary held directory controls three trials that move it through three already-known
/// destination subtrees. Each post-move child lookup is offline, so only the rebased id table can
/// answer it.
// contract: C7, C12
#[test]
fn repeated_rebases_replace_cached_destination_subtrees() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let control = lab.mount_path("a").join("stationary");
    fs::create_dir(&control)?;
    for child in 0..SWITCHES {
        std::os::unix::fs::symlink("control", control.join(format!("child-{child}")))?;
    }
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(lab.mount_path("b").join("stationary"))?;
    list(&held)?;
    lab.cut("b", Links::Authority)?;
    ensure!(readlink_entry(&held, "child-0")?.as_os_str() == "control");
    lab.restore("b", Links::Authority)?;
    drop(lab);

    for trial in 0..TRIALS {
        ensure!(rebase_over_decoys(trial)?.is_some(), "lab binaries disappeared");
    }
    Ok(())
}

/// Keeps one direct-I/O descriptor while a optionally renames its entry before each new version.
fn follow_switches(trial: usize, moving: bool) -> Result<Option<()>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let mut name = format!("versions-{moving}-{trial}-0");
    let initial = random_bytes(700 << 10);
    save(&a.join(&name), &initial)?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(b.join(&name))?;
    assert_bytes(&read_at(&held, 0, initial.len())?, &initial, "initial version");

    for switch in 0..SWITCHES {
        let len = match switch {
            0 => 5 << 20,
            1 => 257 << 10,
            _ => 3 << 20,
        };
        let bytes = random_bytes(len);
        save(&a.join(&name), &bytes)?;
        if moving {
            let next = format!("versions-{moving}-{trial}-{}", switch + 1);
            fs::rename(a.join(&name), a.join(&next))?;
            save(&a.join(&name), b"replacement canary")?;
            name = next;
        }
        thread::sleep(VISIBLE_AFTER);
        ensure!(held.metadata()?.len() == len as u64, "switch {switch} size");
        assert_bytes(&read_at(&held, 0, len)?, &bytes, "held direct-I/O version");
    }
    Ok(Some(()))
}

/// A stationary handle controls three handles whose entries alternate grow/shrink versions and
/// renames, with each old path reused by a canary entry.
// contract: C2, C3, C7
#[test]
fn direct_io_handles_follow_repeated_renames_and_version_switches() -> Result<()> {
    if follow_switches(0, false)?.is_none() {
        return Ok(());
    }
    for trial in 0..TRIALS {
        ensure!(
            follow_switches(trial, true)?.is_some(),
            "lab binaries disappeared"
        );
    }
    Ok(())
}

/// Multiple offline seals for one entry collapse to the latest durable copy. This leaves the
/// parent fixed for the control and moves each attacked parent between seals.
// contract: C4a, C7, C12
#[test]
fn crash_recovery_keeps_the_latest_seal_across_repeated_parent_moves() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a").to_owned(), lab.mount_path("b").to_owned());
    let cases = 0..=TRIALS;
    for case in cases.clone() {
        let parent = format!("recovery-{case}-0");
        fs::create_dir(a.join(&parent))?;
        save(&a.join(&parent).join("file"), b"initial")?;
    }
    thread::sleep(VISIBLE_AFTER);

    let held = cases
        .clone()
        .map(|case| File::open(b.join(format!("recovery-{case}-0"))))
        .collect::<std::io::Result<Vec<_>>>()?;
    for dir in &held {
        assert_bytes(&read_entry(dir, "file")?, b"initial", "online recovery control");
    }

    lab.cut("b", Links::Authority)?;
    let mut current = cases
        .clone()
        .map(|case| format!("recovery-{case}-0"))
        .collect::<Vec<_>>();
    for seal in 0..SWITCHES {
        for case in cases.clone() {
            write_entry(&held[case], "file", format!("latest-{case}-{seal}").as_bytes())?;
            if case == 0 {
                continue;
            }
            let next = format!("recovery-{case}-{}", seal + 1);
            fs::rename(a.join(&current[case]), a.join(&next))?;
            fs::create_dir(a.join(&current[case]))?;
            save(&a.join(&current[case]).join("file"), b"replacement canary")?;
            current[case] = next;
        }
    }
    ensure!(
        lab.stats("b")?["pending_commits"] == 4,
        "all writes are journaled"
    );
    lab.kill_client("b")?;
    lab.restore("b", Links::Authority)?;
    lab.restart_client("b")?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);

    for case in cases {
        let expected = format!("latest-{case}-{}", SWITCHES - 1);
        assert_bytes(
            &fs::read(a.join(&current[case]).join("file"))?,
            expected.as_bytes(),
            "latest recovered seal",
        );
        if case != 0 {
            assert_bytes(
                &fs::read(a.join(format!("recovery-{case}-0/file")))?,
                b"replacement canary",
                "replacement at the first old path",
            );
        }
    }
    Ok(())
}

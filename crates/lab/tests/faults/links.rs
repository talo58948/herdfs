//! F3 and F4: a client's store link or authority link cut for a while.

use std::fs::{self, File};
use std::io::{self, Write};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use herdfs_lab::{Lab, Links};

use crate::{VISIBLE_AFTER, assert_history, assert_reads, commit, random_bytes, two_clients, wait_committed};

const STORE_CUT: Duration = Duration::from_secs(30);
const OUTAGE: Duration = Duration::from_secs(60);
/// Files written during an outage.
const FILES: usize = 5;

/// F3: with its store link cut for 30 s, a client keeps reading what it has cached and keeps
/// committing, since uploads go through the authority.
// contract: C2, C11
#[test]
fn f3_a_cut_store_link_keeps_cached_reads_and_commits_working() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (cached, cold) = (random_bytes(4 << 20), random_bytes(2 << 20));
    commit(&lab, "a", "cached.bin", &cached)?;
    commit(&lab, "a", "cold.bin", &cold)?;
    thread::sleep(VISIBLE_AFTER);
    assert_reads(&lab, "b", "cached.bin", &cached)?;
    lab.cut("b", Links::Store)?;
    let cold_read = fs::read(lab.mount_path("b").join("cold.bin"));
    assert!(cold_read.is_err(), "uncached chunks come through a cut link");
    let deadline = Instant::now() + STORE_CUT;
    let mut saved = Vec::new();
    while Instant::now() < deadline {
        assert_reads(&lab, "b", "cached.bin", &cached)?;
        let (name, bytes) = (format!("during-cut-{}.bin", saved.len()), random_bytes(512 << 10));
        commit(&lab, "b", &name, &bytes)?;
        saved.push((name, bytes));
        thread::sleep(Duration::from_secs(2));
    }
    lab.restore("b", Links::Store)?;
    assert_reads(&lab, "b", "cold.bin", &cold)?;
    thread::sleep(VISIBLE_AFTER);
    for (name, bytes) in &saved {
        assert_reads(&lab, "a", name, bytes)?;
    }
    Ok(())
}

/// Files `f-0` to `f-4`, each committed through client `a` with its bytes.
fn committed_files(lab: &Lab) -> Result<(Vec<String>, Vec<Vec<u8>>)> {
    let names: Vec<String> = (0..FILES).map(|index| format!("f-{index}.txt")).collect();
    let contents: Vec<Vec<u8>> = names.iter().map(|_| random_bytes(256 << 10)).collect();
    for (name, bytes) in names.iter().zip(&contents) {
        commit(lab, "a", name, bytes)?;
    }
    thread::sleep(VISIBLE_AFTER);
    Ok((names, contents))
}

/// F4: files client `a` opened before losing the authority for 60 s take writes during the
/// outage, and those commit once on reconnect, after a commit `b` made meanwhile (C5: the last
/// to arrive is the head, and all stay in history).
// contract: C12, C5
#[test]
fn f4_writes_through_files_open_across_an_authority_outage_commit_on_reconnect() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (names, before) = committed_files(&lab)?;
    let open = |name: &String| File::create(lab.mount_path("a").join(name));
    let files = names.iter().map(open).collect::<io::Result<Vec<_>>>()?;
    lab.cut("a", Links::Authority)?;
    let offline: Vec<Vec<u8>> = names.iter().map(|_| random_bytes(256 << 10)).collect();
    for (mut file, bytes) in files.into_iter().zip(&offline) {
        file.write_all(bytes)?;
    }
    let from_b = random_bytes(256 << 10);
    commit(&lab, "b", &names[0], &from_b)?;
    thread::sleep(OUTAGE);
    assert_eq!(
        lab.stats("a")?["pending_commits"],
        FILES,
        "a's writes wait in its journal"
    );
    lab.restore("a", Links::Authority)?;
    wait_committed(&lab, "a")?;
    thread::sleep(VISIBLE_AFTER);
    assert_history(&lab, &names[0], &[&before[0], &from_b, &offline[0]])?;
    for ((name, before), offline) in names.iter().zip(&before).zip(&offline).skip(1) {
        assert_history(&lab, name, &[before, offline])?;
    }
    for (name, offline) in names.iter().zip(&offline) {
        assert_reads(&lab, "b", name, offline)?;
    }
    Ok(())
}

/// F4 as a user meets it: files written while the authority is unreachable.
// contract: C12
#[test]
fn f4_files_written_during_an_authority_outage_commit_on_reconnect() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (names, before) = committed_files(&lab)?;
    lab.cut("a", Links::Authority)?;
    let offline: Vec<Vec<u8>> = names.iter().map(|_| random_bytes(256 << 10)).collect();
    for (name, bytes) in names.iter().zip(&offline) {
        fs::write(lab.mount_path("a").join(name), bytes)?;
    }
    thread::sleep(OUTAGE);
    lab.restore("a", Links::Authority)?;
    wait_committed(&lab, "a")?;
    thread::sleep(VISIBLE_AFTER);
    for ((name, before), offline) in names.iter().zip(&before).zip(&offline) {
        assert_history(&lab, name, &[before, offline])?;
        assert_reads(&lab, "b", name, offline)?;
    }
    Ok(())
}

/// F4 through a silent outage, as a real network drops packets rather than refusing them: once
/// one call has timed out, the client answers from what it holds at once instead of waiting out
/// a timeout on every call, and the writes still commit on reconnect.
// contract: C12
#[test]
fn f4_a_silent_outage_answers_cached_opens_at_once_and_commits_on_reconnect() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (names, before) = committed_files(&lab)?;
    for (name, bytes) in names.iter().zip(&before) {
        assert_reads(&lab, "a", name, bytes)?;
    }
    lab.silence("a", Links::Authority)?;
    // The first call that needs the authority times out, and tells the client it is gone.
    assert!(
        fs::read_dir(lab.mount_path("a")).is_err(),
        "listing needs the authority"
    );
    let started = Instant::now();
    assert_reads(&lab, "a", &names[0], &before[0])?;
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "a cached open and read took {took:?}"
    );
    let offline: Vec<Vec<u8>> = names.iter().map(|_| random_bytes(256 << 10)).collect();
    for (name, bytes) in names.iter().zip(&offline) {
        fs::write(lab.mount_path("a").join(name), bytes)?;
    }
    thread::sleep(OUTAGE);
    lab.restore("a", Links::Authority)?;
    wait_committed(&lab, "a")?;
    thread::sleep(VISIBLE_AFTER);
    for ((name, before), offline) in names.iter().zip(&before).zip(&offline) {
        assert_history(&lab, name, &[before, offline])?;
        assert_reads(&lab, "b", name, offline)?;
    }
    Ok(())
}

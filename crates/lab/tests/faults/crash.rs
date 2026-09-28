//! F1: SIGKILL a client mid-write and mid-commit.

use std::fs::{self, File};
use std::io::Write;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use herdfs_core::chunk;
use herdfs_lab::{Lab, run};

use crate::{
    VISIBLE_AFTER, assert_history, assert_reads, chunks_of, commit, history, random_bytes, two_clients,
    wait_committed, wait_until,
};

/// The clients of `two_clients`.
const WRITER: &str = "a";
const READER: &str = "b";
const NAME: &str = "big.bin";
/// Large enough that chunking and uploading it take far longer than a SIGKILL.
const BIG: usize = 32 << 20;
/// How far an unclosed rewrite gets before its client is killed.
const KILL_AT: u64 = 16 << 20;
/// Commits that grow the file in `a_reader_sees_a_file_that_grows_whole`.
const GROWTHS: usize = 6;

/// Reads `NAME` whole on the reader's mount, over and over until finished, and fails on any read
/// that is not exactly one of `versions` (C3).
struct Watcher {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<usize>>,
}

impl Watcher {
    fn start(lab: &Lab, versions: &[&[u8]]) -> Self {
        let path = lab.mount_path(READER).join(NAME);
        let versions: Vec<Vec<u8>> = versions.iter().map(|bytes| bytes.to_vec()).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = thread::spawn(move || {
            let mut reads = 0;
            while !stopped.load(Ordering::Relaxed) {
                let bytes = fs::read(&path)?;
                ensure!(
                    versions.contains(&bytes),
                    "a reader saw {}",
                    torn(&bytes, &versions)
                );
                reads += 1;
            }
            Ok(reads)
        });
        Self { stop, thread }
    }

    /// Stops watching; fails if any read did or none happened.
    fn finish(self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        let joined = self.thread.join();
        let reads = joined.map_err(|_| anyhow!("the watcher panicked"))??;
        ensure!(reads > 0, "the watcher read nothing");
        Ok(())
    }
}

/// What a read that is no whole version holds, for the failure message.
fn torn(bytes: &[u8], versions: &[Vec<u8>]) -> String {
    let len = bytes.len();
    let prefix_of = versions.iter().position(|version| version.starts_with(bytes));
    match prefix_of {
        Some(index) => format!("the first {len} bytes of version {index} only"),
        None => format!("{len} bytes that are no version written"),
    }
}

/// Rewrites `NAME` on the writer without closing it, and SIGKILLs the writer's mount once the
/// rewrite is `KILL_AT` bytes in.
fn kill_mid_write(lab: &mut Lab) -> Result<()> {
    let path = lab.mount_path(WRITER).join(NAME);
    let written = Arc::new(AtomicU64::new(0));
    let progress = written.clone();
    let writer = thread::spawn(move || -> std::io::Result<()> {
        let block = random_bytes(1 << 20);
        let mut file = File::create(path)?;
        // Bounded, so a kill that never comes cannot fill the disk.
        for _ in 0..512 {
            file.write_all(&block)?;
            progress.fetch_add(block.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    });
    wait_until("the rewrite gets under way", Duration::from_secs(30), || {
        Ok(written.load(Ordering::Relaxed) >= KILL_AT)
    })?;
    lab.kill_client(WRITER)?;
    let finished = writer.join().map_err(|_| anyhow!("the writer panicked"))?;
    ensure!(finished.is_err(), "the rewrite finished before the kill");
    Ok(())
}

/// Restarts the writer and waits until its journal is committed and visible to others.
fn recover(lab: &mut Lab) -> Result<()> {
    lab.restart_client(WRITER)?;
    wait_committed(lab, WRITER)?;
    thread::sleep(VISIBLE_AFTER);
    Ok(())
}

// contract: C3, C4a
#[test]
fn f1_a_client_killed_mid_write_keeps_its_last_commit() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let first = random_bytes(8 << 20);
    commit(&lab, WRITER, NAME, &first)?;
    thread::sleep(VISIBLE_AFTER);
    let watcher = Watcher::start(&lab, &[&first]);
    kill_mid_write(&mut lab)?;
    recover(&mut lab)?;
    for client in [WRITER, READER] {
        assert_reads(&lab, client, NAME, &first)?;
    }
    assert_history(&lab, NAME, &[&first])?;
    watcher.finish()
}

// contract: C4a, C3
#[test]
fn f1_a_client_killed_mid_rewrite_still_commits_the_write_it_closed() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let (first, closed) = (random_bytes(8 << 20), random_bytes(8 << 20));
    commit(&lab, WRITER, NAME, &first)?;
    thread::sleep(VISIBLE_AFTER);
    let watcher = Watcher::start(&lab, &[&first, &closed]);
    // Closed without fsync, so its commit is still under way when the rewrite starts.
    fs::write(lab.mount_path(WRITER).join(NAME), &closed)?;
    kill_mid_write(&mut lab)?;
    recover(&mut lab)?;
    for client in [WRITER, READER] {
        assert_reads(&lab, client, NAME, &closed)?;
    }
    assert_history(&lab, NAME, &[&first, &closed])?;
    watcher.finish()
}

/// The fault-free core of the test above, and its minimal repro.
// contract: C3
#[test]
fn a_rewrite_while_a_closed_write_commits_publishes_only_whole_versions() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (first, second) = (random_bytes(BIG), random_bytes(BIG));
    let path = lab.mount_path(WRITER).join(NAME);
    fs::write(&path, &first)?;
    fs::write(&path, &second)?;
    wait_committed(&lab, WRITER)?;
    // The first write may be superseded before it commits, but nothing else may appear.
    let whole = [chunks_of(&first)?, chunks_of(&second)?];
    let versions = history(&lab, NAME)?;
    let sizes: Vec<u64> = versions.iter().map(|chunks| chunk::total_len(chunks)).collect();
    assert!(
        versions.iter().all(|chunks| whole.contains(chunks)),
        "versions of {sizes:?} bytes"
    );
    assert_eq!(versions.last(), Some(&whole[1]), "versions of {sizes:?} bytes");
    Ok(())
}

/// A reader that keeps opening a file while larger versions it has not read yet land: the
/// minimal repro of the partial reads `f1_a_client_killed_before_its_commit_lands` meets.
// contract: C3
#[test]
fn a_reader_sees_a_file_that_grows_whole() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let small = random_bytes(8 << 20);
    let larger: Vec<Vec<u8>> = (0..GROWTHS).map(|_| random_bytes(BIG)).collect();
    commit(&lab, WRITER, NAME, &small)?;
    thread::sleep(VISIBLE_AFTER);
    let versions: Vec<&[u8]> = larger.iter().map(Vec::as_slice).chain([&small[..]]).collect();
    let watcher = Watcher::start(&lab, &versions);
    for large in &larger {
        commit(&lab, WRITER, NAME, large)?;
        thread::sleep(VISIBLE_AFTER);
        commit(&lab, WRITER, NAME, &small)?;
    }
    watcher.finish()
}

/// A shell rewrites an existing file through a duplicated descriptor, as `>` and `dd` do: the
/// shell opens it truncating, dups it onto stdout and closes the original, which flushes the
/// empty file before any data arrives. Only the version with the data commits, and a reader on
/// the other client never sees an empty one.
// contract: C3
#[test]
fn a_rewrite_through_a_dup_d_descriptor_commits_one_version() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (first, data) = (random_bytes(1 << 20), random_bytes(1 << 20));
    commit(&lab, WRITER, NAME, &first)?;
    thread::sleep(VISIBLE_AFTER);
    let watcher = Watcher::start(&lab, &[&first, &data]);
    let source = tempfile::NamedTempFile::new()?;
    fs::write(source.path(), &data)?;
    let mut shell = Command::new("bash");
    shell.args(["-c", "cat \"$2\" > \"$1\"", "bash"]);
    run(shell.arg(lab.mount_path(WRITER).join(NAME)).arg(source.path()))?;
    wait_committed(&lab, WRITER)?;
    thread::sleep(VISIBLE_AFTER);
    assert_history(&lab, NAME, &[&first, &data])?;
    watcher.finish()
}

// contract: C4a, C3
#[test]
fn f1_a_client_killed_before_its_commit_lands_commits_after_restart() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let (first, closed) = (random_bytes(8 << 20), random_bytes(BIG));
    commit(&lab, WRITER, NAME, &first)?;
    thread::sleep(VISIBLE_AFTER);
    let watcher = Watcher::start(&lab, &[&first, &closed]);
    // close returns once the write is journaled; chunking and uploading it take far longer.
    fs::write(lab.mount_path(WRITER).join(NAME), &closed)?;
    lab.kill_client(WRITER)?;
    assert_history(&lab, NAME, &[&first])?;
    recover(&mut lab)?;
    for client in [WRITER, READER] {
        assert_reads(&lab, client, NAME, &closed)?;
    }
    assert_history(&lab, NAME, &[&first, &closed])?;
    watcher.finish()
}

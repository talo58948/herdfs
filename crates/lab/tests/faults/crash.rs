//! F1 and F2: SIGKILL a client mid-write and mid-commit, and the authority while two clients
//! commit.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use herdfs_core::chunk;
use herdfs_lab::Lab;

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
/// Authority crashes in F2.
const KILLS: usize = 8;

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
#[ignore = "BUG: a rewrite truncates the journal copy that a closed write is still committing from"]
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
#[ignore = "BUG: a rewrite truncates the journal copy that a closed write is still committing from"]
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
#[ignore = "BUG: an open that finds a longer version can read only the old length, so readers see partial versions"]
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

// contract: C4a, C3
#[test]
#[ignore = "BUG: an open that finds a longer version can read only the old length, so readers see partial versions"]
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

/// A file a writer closed, and whether its fsync saw the commit.
struct Written {
    name: String,
    bytes: Vec<u8>,
    acknowledged: bool,
}

/// Writes new files on `mount`, one at a time and each synced, until `stop`.
fn write_files(mount: &Path, prefix: &str, stop: &AtomicBool) -> Result<Vec<Written>> {
    let mut written = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let name = format!("{prefix}-{}.bin", written.len());
        let bytes = random_bytes(rand::random_range(64 << 10..512 << 10));
        // Creating a file needs the authority; while it is down, try again.
        let Ok(mut file) = File::create(mount.join(&name)) else {
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        file.write_all(&bytes)?;
        let acknowledged = file.sync_all().is_ok();
        written.push(Written {
            name,
            bytes,
            acknowledged,
        });
    }
    Ok(written)
}

// contract: C4b, C3
#[test]
fn f2_commits_survive_authority_crashes_around_them() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let stop = Arc::new(AtomicBool::new(false));
    let writers = [WRITER, READER].map(|client| {
        let (mount, stop) = (lab.mount_path(client).to_owned(), stop.clone());
        thread::spawn(move || write_files(&mount, client, &stop))
    });
    for _ in 0..KILLS {
        thread::sleep(Duration::from_millis(rand::random_range(200..800)));
        lab.kill_authority();
        thread::sleep(Duration::from_millis(rand::random_range(50..300)));
        lab.restart_authority()?;
    }
    stop.store(true, Ordering::Relaxed);
    let mut written = Vec::new();
    for (client, writer) in [WRITER, READER].into_iter().zip(writers) {
        let joined = writer.join();
        let files = joined.map_err(|_| anyhow!("{client}'s writer panicked"))??;
        written.extend(files.into_iter().map(|file| (client, file)));
    }
    let acknowledged = written.iter().filter(|(_, file)| file.acknowledged).count();
    eprintln!("{} files closed, {acknowledged} acknowledged", written.len());
    ensure!(acknowledged > 0, "no commit was acknowledged");
    for client in [WRITER, READER] {
        wait_committed(&lab, client)?;
    }
    thread::sleep(VISIBLE_AFTER);
    // Every closed file ends as one whole version, readable on the other client; for the
    // acknowledged ones that is C4b, for the rest C4a.
    for (client, file) in &written {
        assert_history(&lab, &file.name, &[&file.bytes])?;
        let other = if *client == WRITER { READER } else { WRITER };
        assert_reads(&lab, other, &file.name, &file.bytes)?;
    }
    Ok(())
}

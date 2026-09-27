//! F5 and F7: reads over a bad link with every strategy, and reads through a cache far smaller
//! than what they touch.

use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::thread;

use anyhow::{Context, Result, anyhow};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, commit, random_bytes, start};

/// Random range reads per file.
const RANGES: usize = 20;
const MAX_RANGE: usize = 300 << 10;
/// F5's readers, named after their strategies as the stats name them.
const READERS: [(&str, Strategy); 3] = [
    ("stream", Strategy::Stream),
    ("fetch", Strategy::Fetch),
    ("pin", Strategy::Pin),
];
/// F7's cache limit, and the files it reads: nine times as many bytes.
const SMALL_CACHE: u64 = 1 << 20;
const FILES: usize = 6;
const FILE_LEN: usize = 1536 << 10;

/// Reads `path` at random ranges, some across chunk boundaries or past the end, then whole, and
/// compares every read with `expected`.
fn check_reads(path: &Path, expected: &[u8]) -> Result<()> {
    let file = File::open(path)?;
    let end = expected.len();
    for _ in 0..RANGES {
        let (off, len) = (
            rand::random_range(0..end + 1024),
            rand::random_range(1..MAX_RANGE),
        );
        let read = read_range(&file, off, len)?;
        let written = &expected[off.min(end)..(off + len).min(end)];
        assert!(
            read == written,
            "{}: bytes [{off}, {off} + {len}) differ",
            path.display()
        );
    }
    let whole = fs::read(path)?;
    let (got, wanted) = (whole.len(), expected.len());
    assert!(
        whole == expected,
        "{}: reads {got} bytes, not the {wanted} written",
        path.display()
    );
    Ok(())
}

/// Up to `len` bytes at `off`; fewer only at the end of the file.
fn read_range(file: &File, off: usize, len: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = vec![0; len];
    let mut filled = 0;
    while filled < len {
        let read = file.read_at(&mut bytes[filled..], (off + filled) as u64)?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    bytes.truncate(filled);
    Ok(bytes)
}

// contract: C8
#[test]
fn f5_every_strategy_reads_the_same_bytes_over_a_bad_link() -> Result<()> {
    let writer = LabConfig::default().client("writer", "alice", &[], Profile::Lan, Strategy::Stream);
    let config = READERS.iter().fold(writer, |config, &(name, strategy)| {
        config.client(name, "bob", &[], Profile::Bad, strategy)
    });
    let Some(lab) = start(config)? else {
        return Ok(());
    };
    let bytes = random_bytes(2 << 20);
    commit(&lab, "writer", "file.bin", &bytes)?;
    thread::sleep(VISIBLE_AFTER);
    thread::scope(|scope| {
        let readers: Vec<_> = (READERS.iter())
            .map(|(name, _)| {
                let (path, bytes) = (lab.mount_path(name).join("file.bin"), &bytes);
                scope.spawn(move || check_reads(&path, bytes))
            })
            .collect();
        let join = |reader: thread::ScopedJoinHandle<Result<()>>| {
            reader.join().map_err(|_| anyhow!("a reader panicked"))?
        };
        readers.into_iter().try_for_each(join)
    })?;
    for (name, _) in READERS {
        let decided = lab.stats(name)?["decisions"][name].as_u64();
        assert!(decided >= Some(1), "{name} reads with its strategy");
    }
    Ok(())
}

/// The client keeps recent chunks in memory above its disk cache, so the reader restarts
/// between passes: after that, every chunk comes from the small disk cache or the store.
// contract: C11
#[test]
fn f7_a_small_cache_evicts_under_load_and_serves_only_the_right_bytes() -> Result<()> {
    let config = LabConfig::default()
        .client("writer", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("reader", "bob", &[], Profile::Lan, Strategy::Chaos(7))
        .cache_bytes("reader", SMALL_CACHE);
    let Some(mut lab) = start(config)? else {
        return Ok(());
    };
    let mut files: Vec<(String, Vec<u8>)> = (0..FILES)
        .map(|index| (format!("f-{index}.bin"), random_bytes(FILE_LEN)))
        .collect();
    for (name, bytes) in &files {
        commit(&lab, "writer", name, bytes)?;
    }
    thread::sleep(VISIBLE_AFTER);
    check_all(&lab, &files)?;
    lab.restart_client("reader")?;
    check_all(&lab, &files)?;
    let evicted = (FILES * FILE_LEN) as u64 - SMALL_CACHE;
    let fetched = bytes_fetched(&lab)?;
    assert!(
        fetched >= evicted,
        "{fetched} bytes fetched again: the cache kept more than it may"
    );
    // New versions of half the files, whose old chunks the cache may still hold.
    for (name, bytes) in files.iter_mut().step_by(2) {
        *bytes = random_bytes(FILE_LEN);
        commit(&lab, "writer", name, bytes)?;
    }
    thread::sleep(VISIBLE_AFTER);
    check_all(&lab, &files)?;
    // Background prefetches of the last opens finish before the cache is measured.
    thread::sleep(VISIBLE_AFTER);
    let cache = fs::read_dir(lab.client_dir("reader").join("cache"))?;
    let cached = cache
        .map(|entry| Ok(entry?.metadata()?.len()))
        .sum::<Result<u64>>()?;
    assert!(cached <= SMALL_CACHE, "the cache holds {cached} bytes");
    let stats = lab.stats("reader")?;
    for (strategy, _) in READERS {
        assert!(
            stats["decisions"][strategy].as_u64() >= Some(1),
            "no {strategy} open"
        );
    }
    Ok(())
}

/// Checks every file's reads on the reader.
fn check_all(lab: &Lab, files: &[(String, Vec<u8>)]) -> Result<()> {
    let reader = lab.mount_path("reader");
    files
        .iter()
        .try_for_each(|(name, bytes)| check_reads(&reader.join(name), bytes))
}

fn bytes_fetched(lab: &Lab) -> Result<u64> {
    let stats = lab.stats("reader")?;
    stats["bytes_fetched"]
        .as_u64()
        .context("stats count fetched bytes")
}

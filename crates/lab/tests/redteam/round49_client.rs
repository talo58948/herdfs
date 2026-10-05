//! Round 49: client cache publication, versioned grants, and held-id journal composition.

use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::sync::Barrier;
use std::thread;

use anyhow::{Context, Result, ensure};
use herdfs_core::proto::Version;
use herdfs_lab::{ADMIN, Lab, LabConfig, Links, Profile, Strategy};

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, read_at, save, two_clients, wait_pending};

const TRIALS: usize = 3;
const PAGE: usize = 4096;

fn count(lab: &Lab, name: &str) -> Result<u64> {
    lab.stats("b")?[name]
        .as_u64()
        .with_context(|| format!("stats count {name}"))
}

fn concurrent_page(file: &File, expected: &[u8]) -> Result<()> {
    const READERS: usize = 8;
    let start = Barrier::new(READERS);
    thread::scope(|scope| {
        let reads = (0..READERS)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    read_at(file, 0, PAGE)
                })
            })
            .collect::<Vec<_>>();
        for read in reads {
            let bytes = read
                .join()
                .map_err(|_| anyhow::anyhow!("read thread panicked"))??;
            assert_bytes(&bytes, &expected[..PAGE], "concurrent page");
        }
        Ok(())
    })
}

fn cache_lab() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
        .cache_bytes("b", 16 << 20);
    Lab::start(config).map(Some)
}

fn read_with_destination(lab: &Lab, name: &str, bytes: &[u8], block: bool) -> Result<u64> {
    save(&lab.mount_path("a").join(name), bytes)?;
    let version = lab.head(ADMIN, name)?;
    let first = version.chunks.first().context("fixture has no chunks")?.id;
    let destination = lab.client_dir("b").join("cache").join(first.to_string());
    if block {
        fs::create_dir(&destination)?;
    }
    thread::sleep(VISIBLE_AFTER);
    let before = count(lab, "chunks_fetched")?;
    let file = File::open(lab.mount_path("b").join(name))?;
    concurrent_page(&file, bytes)?;
    let fetched = count(lab, "chunks_fetched")? - before;
    if block {
        ensure!(destination.is_dir(), "failed persist replaced its blocker");
    } else {
        ensure!(destination.is_file(), "control chunk was not cached");
    }
    Ok(fetched)
}

/// A normal cold publication controls three concurrent cold reads whose verified chunk cannot
/// replace its cache destination. Reads still coalesce and no self-deleting temp file remains.
// contract: C9, C10, C11
#[test]
fn failed_cache_publication_keeps_concurrent_reads_correct_and_clean() -> Result<()> {
    let Some(lab) = cache_lab()? else {
        return Ok(());
    };
    let control = random_bytes(2 << 20);
    ensure!(
        read_with_destination(&lab, "cache-control", &control, false)? == 1,
        "control fetched its first chunk once"
    );
    for trial in 0..TRIALS {
        let bytes = random_bytes(2 << 20);
        let fetched = read_with_destination(&lab, &format!("cache-failure-{trial}"), &bytes, true)?;
        ensure!(
            fetched == 1,
            "trial {trial} fetched the shared chunk {fetched} times"
        );
    }
    let cache = lab.client_dir("b").join("cache");
    let temps = fs::read_dir(cache)?.try_fold(0, |temps, entry| {
        let temp = entry?.file_name().to_string_lossy().ends_with(".tmp");
        Ok::<_, std::io::Error>(temps + usize::from(temp))
    })?;
    ensure!(temps == 0, "failed publications left {temps} temp files");
    Ok(())
}

fn starts(version: &Version, bytes: &[u8]) -> Result<Vec<(u64, u8)>> {
    ensure!(
        version.chunks.len() >= TRIALS,
        "fixture has fewer than {TRIALS} chunks"
    );
    let mut offset = 0_u64;
    Ok(version
        .chunks
        .iter()
        .take(TRIALS)
        .map(|chunk| {
            let start = (offset, bytes[offset as usize]);
            offset += u64::from(chunk.len);
            start
        })
        .collect())
}

fn read_starts(file: &File, starts: &[(u64, u8)]) -> Result<()> {
    thread::scope(|scope| {
        let reads = starts
            .iter()
            .map(|&(offset, expected)| scope.spawn(move || (read_at(file, offset, 1), expected)))
            .collect::<Vec<_>>();
        for read in reads {
            let (bytes, expected) = read.join().map_err(|_| anyhow::anyhow!("read thread panicked"))?;
            ensure!(bytes? == [expected], "wrong byte at a chunk boundary");
        }
        Ok(())
    })
}

/// Repeating concurrent reads of one version controls three head switches on one held handle.
/// Each switch needs fresh version-scoped grants, and repeats reuse only that version's URLs.
// contract: C3, C8, C9, S2
#[test]
fn concurrent_reads_do_not_reuse_grants_across_version_switches() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
        .cache_bytes("b", 0);
    let lab = Lab::start(config)?;
    let path_a = lab.mount_path("a").join("switching-grants");
    let path_b = lab.mount_path("b").join("switching-grants");
    let mut bytes = random_bytes(9 << 20);
    save(&path_a, &bytes)?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(&path_b)?;

    let mut grants = count(&lab, "grants")?;
    for version_number in 0..=TRIALS {
        if version_number != 0 {
            bytes = random_bytes(9 << 20);
            save(&path_a, &bytes)?;
            thread::sleep(VISIBLE_AFTER);
            ensure!(held.metadata()?.len() == bytes.len() as u64, "switch metadata");
        }
        let version = lab.head(ADMIN, "switching-grants")?;
        read_starts(&held, &starts(&version, &bytes)?)?;
        let fresh = count(&lab, "grants")?;
        ensure!(fresh > grants, "version {version_number} reused an older grant");
        read_starts(&held, &starts(&version, &bytes)?)?;
        ensure!(count(&lab, "grants")? == fresh, "live grants were not reused");
        grants = fresh;
    }
    Ok(())
}

fn layered_held_write(lab: &mut Lab, trial: usize, moved: bool) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let original = format!("held-layer-{moved}-{trial}");
    let relocated = format!("held-layer-moved-{trial}");
    let (a, b) = (lab.mount_path("a").to_owned(), lab.mount_path("b").to_owned());
    save(&a.join(&original), b"abcdefghij")?;
    thread::sleep(VISIBLE_AFTER);
    let first = File::options().read(true).write(true).open(b.join(&original))?;
    let second = File::options().read(true).write(true).open(b.join(&original))?;
    let syncer = File::open(b.join(&original))?;
    first.write_all_at(b"FIRST", 0)?;
    second.set_len(8)?;
    syncer.sync_all()?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(&fs::read(a.join(&original))?, b"FIRSTfgh", "sibling fsync stage");

    let authoritative = if moved {
        fs::rename(a.join(&original), a.join(&relocated))?;
        save(&a.join(&original), b"replacement canary")?;
        a.join(&relocated)
    } else {
        a.join(&original)
    };
    lab.cut("b", Links::Authority)?;
    second.write_all_at(b"SECOND", 2)?;
    first.set_len(6)?;
    drop((syncer, second, first));
    wait_pending(lab)?;
    lab.kill_client("b")?;
    lab.restore("b", Links::Authority)?;
    lab.restart_client("b")?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    let replacement = moved.then(|| fs::read(a.join(&original))).transpose()?;
    Ok((fs::read(authoritative)?, replacement))
}

/// A stationary entry controls three entries renamed and replaced after sibling fsync. Further
/// offline changes through the old descriptors survive a crash and land on the held entry id.
// contract: C1, C2, C3, C4a, C7, C12
#[test]
fn layered_changes_keep_their_held_id_across_fsync_rename_and_recovery() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let (control, None) = layered_held_write(&mut lab, 0, false)? else {
        unreachable!("stationary control has no replacement")
    };
    assert_bytes(&control, b"FISECO", "stationary control");
    for trial in 0..TRIALS {
        let (held, replacement) = layered_held_write(&mut lab, trial, true)?;
        assert_bytes(&held, b"FISECO", "recovered held entry");
        assert_bytes(
            &replacement.context("moved trial has a replacement")?,
            b"replacement canary",
            "replacement entry",
        );
    }
    Ok(())
}

//! Round 51: concurrent cache eviction and mixed reuse of version-scoped grant URLs.

use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::sync::Barrier;
use std::thread;

use anyhow::{Context, Result, ensure};
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, read_at, save};

const TRIALS: usize = 3;
const FILES: usize = 12;
const CACHE_BYTES: u64 = 3 << 20;

fn cache_lab(cache_bytes: u64) -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
        .cache_bytes("b", cache_bytes);
    Lab::start(config).map(Some)
}

fn read_set(lab: &Lab, names: &[String], bytes: &[Vec<u8>], concurrent: bool) -> Result<()> {
    if !concurrent {
        for (name, expected) in names.iter().zip(bytes) {
            assert_bytes(
                &fs::read(lab.mount_path("b").join(name))?,
                expected,
                "sequential cache control",
            );
        }
        return Ok(());
    }
    let start = Barrier::new(FILES);
    thread::scope(|scope| {
        let reads = names
            .iter()
            .zip(bytes)
            .map(|(name, expected)| {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    let read = fs::read(lab.mount_path("b").join(name))?;
                    assert_bytes(&read, expected, "concurrent cache churn");
                    Ok::<_, std::io::Error>(())
                })
            })
            .collect::<Vec<_>>();
        for read in reads {
            read.join().map_err(|_| anyhow::anyhow!("reader panicked"))??;
        }
        Ok(())
    })
}

fn cached_bytes(lab: &Lab) -> Result<(u64, usize)> {
    fs::read_dir(lab.client_dir("b").join("cache"))?.try_fold((0, 0), |(bytes, temps), entry| {
        let entry = entry?;
        let temp = entry.file_name().to_string_lossy().ends_with(".tmp");
        Ok((bytes + entry.metadata()?.len(), temps + usize::from(temp)))
    })
}

/// A sequential fill controls three waves in which more distinct chunks than the cache holds
/// publish and evict concurrently. Every read stays correct and the settled cache stays bounded.
// contract: C10, C11
#[test]
fn concurrent_chunk_publication_and_eviction_stay_correct_and_bounded() -> Result<()> {
    let Some(lab) = cache_lab(CACHE_BYTES)? else {
        return Ok(());
    };
    let names = (0..FILES)
        .map(|index| format!("evict-{index}"))
        .collect::<Vec<_>>();
    let bytes = (0..FILES).map(|_| random_bytes(1 << 20)).collect::<Vec<_>>();
    for (name, contents) in names.iter().zip(&bytes) {
        save(&lab.mount_path("a").join(name), contents)?;
    }
    thread::sleep(VISIBLE_AFTER);

    read_set(&lab, &names, &bytes, false)?;
    for trial in 0..TRIALS {
        read_set(&lab, &names, &bytes, true)?;
        let (held, temps) = cached_bytes(&lab)?;
        ensure!(held <= CACHE_BYTES, "trial {trial}: cache holds {held} bytes");
        ensure!(temps == 0, "trial {trial}: cache left {temps} temp files");
    }
    Ok(())
}

fn chunk_start(version: &herdfs_core::proto::Version, index: usize) -> u64 {
    version.chunks[..index]
        .iter()
        .map(|chunk| u64::from(chunk.len))
        .sum()
}

fn rewrite_after_warming(lab: &Lab, name: &str, warm: bool) -> Result<()> {
    let path_a = lab.mount_path("a").join(name);
    let path_b = lab.mount_path("b").join(name);
    // More than three maximum-size chunks, so indices 0 and 2 leave missing slots around them.
    let bytes = random_bytes(25 << 20);
    save(&path_a, &bytes)?;
    let version = lab.head(ADMIN, name)?;
    ensure!(version.chunks.len() >= 4, "fixture has too few chunks");
    thread::sleep(VISIBLE_AFTER);

    if warm {
        let file = File::open(&path_b)?;
        for index in [0, 2] {
            let offset = chunk_start(&version, index);
            ensure!(read_at(&file, offset, 1)? == [bytes[offset as usize]]);
        }
    }

    let mut expected = bytes;
    let changed = expected.len() / 2;
    expected[changed] ^= 0xff;
    let writer = File::options().read(true).write(true).open(&path_b)?;
    writer.write_all_at(&expected[changed..=changed], changed as u64)?;
    writer.sync_all()?;
    drop(writer);
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(&fs::read(path_a)?, &expected, "materialized rewrite");
    Ok(())
}

/// A wholly cold materialization controls three copies whose first grant batch mixes live URLs
/// from earlier reads with newly granted indices. Slotting either set out of order changes bytes.
// contract: C1, S2
#[test]
fn materialization_keeps_mixed_reused_and_new_grant_urls_in_order() -> Result<()> {
    let Some(lab) = cache_lab(0)? else {
        return Ok(());
    };
    rewrite_after_warming(&lab, "grant-order-control", false)?;
    for trial in 0..TRIALS {
        rewrite_after_warming(&lab, &format!("grant-order-{trial}"), true)
            .with_context(|| format!("mixed grant trial {trial}"))?;
    }
    Ok(())
}

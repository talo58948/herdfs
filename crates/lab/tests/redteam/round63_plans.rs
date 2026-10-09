//! Round 63: Parquet prefetch must stay within its bounded metadata region.

use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::proto::Version;
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, random_bytes, save};

const FILE_LEN: usize = 24 << 20;
const FAKE_METADATA: usize = (8 << 20) - 8;
const WAIT: Duration = Duration::from_secs(20);

fn fallback_bytes(version: &Version, read: u64) -> u64 {
    let mut indices = HashSet::new();
    indices.extend(0..version.chunks.len().min(2));
    indices.extend(version.chunks.len().saturating_sub(2)..version.chunks.len());
    let mut offset = 0;
    for (index, chunk) in version.chunks.iter().enumerate() {
        let end = offset + u64::from(chunk.len);
        if offset <= read && read < end {
            indices.insert(index);
        }
        offset = end;
    }
    indices
        .into_iter()
        .map(|index| u64::from(version.chunks[index].len))
        .sum()
}

fn parquet_bytes(version: &Version, read: u64, metadata: u64) -> u64 {
    let ranges = [0..16, read..read + 1, metadata..version.size];
    let mut offset = 0;
    version
        .chunks
        .iter()
        .filter_map(|chunk| {
            let range = offset..offset + u64::from(chunk.len);
            offset = range.end;
            ranges
                .iter()
                .any(|wanted| range.start < wanted.end && wanted.start < range.end)
                .then_some(u64::from(chunk.len))
        })
        .sum()
}

fn wait_fetched(lab: &Lab, name: &str, wanted: u64) -> Result<u64> {
    let deadline = Instant::now() + WAIT;
    loop {
        let fetched = lab.plan("index", name.as_ref())?["bytes_fetched"]
            .as_u64()
            .context("plan lacks bytes_fetched")?;
        if fetched >= wanted || Instant::now() >= deadline {
            return Ok(fetched);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn access(lab: &Lab, name: &str, bytes: &[u8], metadata: Option<u64>) -> Result<(u64, u64)> {
    save(&lab.mount_path("writer").join(name), bytes)?;
    let version = lab.head(ADMIN, name)?;
    thread::sleep(VISIBLE_AFTER);
    let middle = version.size / 2;
    let mut got = [0];
    File::open(lab.mount_path("index").join(name))?.read_exact_at(&mut got, middle)?;
    ensure!(got[0] == bytes[middle as usize], "middle-byte read changed data");
    let bound = metadata.map_or_else(
        || fallback_bytes(&version, middle),
        |metadata| parquet_bytes(&version, middle, metadata),
    );
    Ok((bound, wait_fetched(lab, name, bound)?))
}

fn invalid_parquet(fake: bool) -> Vec<u8> {
    let mut bytes = random_bytes(FILE_LEN);
    if fake {
        bytes[..4].copy_from_slice(b"PAR1");
        let end = bytes.len();
        let metadata = end - 8 - FAKE_METADATA;
        bytes[metadata] = 0;
        bytes[end - 8..end - 4].copy_from_slice(&(FAKE_METADATA as u32).to_le_bytes());
        bytes[end - 4..].copy_from_slice(b"PAR1");
    } else {
        bytes[..16].fill(0x5a);
        bytes[FILE_LEN - 65_557..].fill(0x5a);
    }
    bytes
}

/// An unknown file controls three Parquet-shaped files with the largest accepted metadata region.
/// Prefetch may fetch that region, but no bytes elsewhere in the body.
// contract: P3, C9
#[test]
fn round63_parquet_prefetch_stays_within_metadata() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("writer", "alice", &[], Profile::Lan, Strategy::Stream)
            .client(
                "index",
                "index",
                &[],
                Profile::Lan,
                Strategy::Plan("index/none/no"),
            )
            .on_tmpfs(),
    )?;
    let (bound, fetched) = access(&lab, "parquet-control.bin", &invalid_parquet(false), None)?;
    ensure!(
        fetched == bound,
        "unknown control fetched {fetched}, expected {bound}"
    );

    let mut failures = 0;
    let metadata = (FILE_LEN - 8 - FAKE_METADATA) as u64;
    for trial in 0..3 {
        let name = format!("invalid-parquet-{trial}.bin");
        let (bound, fetched) = access(&lab, &name, &invalid_parquet(true), Some(metadata))?;
        eprintln!("trial {trial}: Parquet fetched {fetched}; metadata bound {bound}");
        failures += usize::from(fetched != bound);
    }
    assert_eq!(
        failures, 0,
        "Parquet indexes that fetched outside their metadata, of 3"
    );
    Ok(())
}

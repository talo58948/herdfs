//! Round 65: hostile index candidates must be validated before remote regions are fetched.

use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::proto::Version;
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, random_bytes, save};

const FILE_LEN: usize = 32 << 20;
const READ_AT: u64 = 24 << 20;
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

fn access(lab: &Lab, name: &str, bytes: &[u8]) -> Result<(u64, u64)> {
    save(&lab.mount_path("writer").join(name), bytes)?;
    let version = lab.head(ADMIN, name)?;
    thread::sleep(VISIBLE_AFTER);
    let mut got = [0];
    File::open(lab.mount_path("index").join(name))?.read_exact_at(&mut got, READ_AT)?;
    ensure!(got[0] == bytes[READ_AT as usize], "probe read changed data");
    let fallback = fallback_bytes(&version, READ_AT);
    Ok((fallback, wait_fetched(lab, name, fallback)?))
}

fn unknown() -> Vec<u8> {
    let mut bytes = random_bytes(FILE_LEN);
    bytes[..16].fill(0x5a);
    bytes[FILE_LEN - 65_557..].fill(0x5a);
    bytes
}

fn invalid_mp4() -> Vec<u8> {
    let mut bytes = unknown();
    let moov = 8 << 20;
    bytes[..4].copy_from_slice(&(moov as u32).to_be_bytes());
    bytes[4..8].copy_from_slice(b"ftyp");
    bytes[8..16].copy_from_slice(b"invalid!");
    bytes[moov..moov + 4].copy_from_slice(&(8_u32 << 20).to_be_bytes());
    bytes[moov + 4..moov + 8].copy_from_slice(b"moov");
    bytes[moov + 8..moov + 12].copy_from_slice(&4_u32.to_be_bytes());
    bytes[moov + 12..moov + 16].copy_from_slice(b"trak");
    bytes
}

/// An unknown-file control bounds three MP4 candidates whose `moov` begins with an impossible
/// four-byte child box. Invalid candidates must use the fallback instead of prefetching that box.
// contract: P3, C9
#[test]
fn round65_invalid_mp4_index_does_not_widen_prefetch() -> Result<()> {
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
    let (bound, fetched) = access(&lab, "index-control.bin", &unknown())?;
    ensure!(
        fetched == bound,
        "unknown control fetched {fetched}, expected {bound}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("invalid-mp4-{trial}.bin");
        let (bound, fetched) = access(&lab, &name, &invalid_mp4())?;
        eprintln!("trial {trial}: invalid MP4 fetched {fetched}; fallback bound {bound}");
        failures += usize::from(fetched > bound);
    }
    assert_eq!(failures, 0, "invalid MP4 indexes that widened prefetch, of 3");
    Ok(())
}

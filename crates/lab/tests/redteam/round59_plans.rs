//! Round 59: invalid format structures must retain bounded fallback prefetch.

use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::proto::Version;
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, random_bytes, save};

const FILE_LEN: usize = 16 << 20;
const WAIT: Duration = Duration::from_secs(20);

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

fn access(lab: &Lab, name: &str, bytes: &[u8]) -> Result<(u64, u64)> {
    save(&lab.mount_path("writer").join(name), bytes)?;
    let version = lab.head(ADMIN, name)?;
    thread::sleep(VISIBLE_AFTER);
    let middle = version.size / 2;
    let mut got = [0];
    File::open(lab.mount_path("index").join(name))?.read_exact_at(&mut got, middle)?;
    ensure!(got[0] == bytes[middle as usize], "middle-byte read changed data");
    let fallback = fallback_bytes(&version, middle);
    Ok((fallback, wait_fetched(lab, name, fallback)?))
}

fn unknown(fake_zip: bool) -> Vec<u8> {
    let mut bytes = random_bytes(FILE_LEN);
    let tail = bytes.len() - 65_557;
    bytes[tail..].fill(0x5a);
    if fake_zip {
        let central = bytes.len() - (7 << 20);
        bytes[central..central + 4].copy_from_slice(b"PK\x01\x02");
        let end = bytes.len();
        bytes[end - 22..end - 18].copy_from_slice(b"PK\x05\x06");
        bytes[end - 18..end - 10].fill(0);
        bytes[end - 10..end - 6].copy_from_slice(&4_u32.to_le_bytes());
        bytes[end - 6..end - 2].copy_from_slice(&(central as u32).to_le_bytes());
        bytes[end - 2..].fill(0);
    }
    bytes
}

/// An ordinary unknown file controls three files with an impossible four-byte ZIP central
/// directory. Invalid ZIP-shaped bytes must fall back to the edge windows, not fetch seven MiB.
// contract: P3, C9
#[test]
fn round59_invalid_zip_directory_does_not_widen_index_prefetch() -> Result<()> {
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
    let (bound, fetched) = access(&lab, "zip-control.bin", &unknown(false))?;
    ensure!(
        fetched == bound,
        "unknown control fetched {fetched}, expected {bound}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("invalid-zip-{trial}.bin");
        let (bound, fetched) = access(&lab, &name, &unknown(true))?;
        eprintln!("trial {trial}: invalid ZIP fetched {fetched}; fallback bound {bound}");
        failures += usize::from(fetched > bound);
    }
    assert_eq!(failures, 0, "invalid ZIP directories that widened prefetch, of 3");
    Ok(())
}

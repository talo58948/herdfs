//! Round 67: ZIP central-directory references must not widen index prefetch.

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
const READ_AT: u64 = 16 << 20;
const ENTRIES: u16 = 112;
const ENTRY_HEADER: usize = 46;
const ENTRY_COMMENT: usize = u16::MAX as usize;
const CENTRAL_LEN: usize = ENTRIES as usize * (ENTRY_HEADER + ENTRY_COMMENT);
const CENTRAL_START: usize = FILE_LEN - 22 - CENTRAL_LEN;
const BODY_TARGET: u32 = 8 << 20;
const WAIT: Duration = Duration::from_secs(20);

fn fallback_bytes(version: &Version) -> u64 {
    let mut indices = HashSet::new();
    indices.extend(0..version.chunks.len().min(2));
    indices.extend(version.chunks.len().saturating_sub(2)..version.chunks.len());
    let mut offset = 0;
    for (index, chunk) in version.chunks.iter().enumerate() {
        let end = offset + u64::from(chunk.len);
        if offset <= READ_AT && READ_AT < end {
            indices.insert(index);
        }
        offset = end;
    }
    indices
        .into_iter()
        .map(|index| u64::from(version.chunks[index].len))
        .sum()
}

fn index_bytes(version: &Version) -> u64 {
    let ranges = [0..16, READ_AT..READ_AT + 1, CENTRAL_START as u64..version.size];
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

fn access(lab: &Lab, name: &str, bytes: &[u8], zip: bool) -> Result<(u64, u64)> {
    save(&lab.mount_path("writer").join(name), bytes)?;
    let version = lab.head(ADMIN, name)?;
    thread::sleep(VISIBLE_AFTER);
    let mut got = [0];
    File::open(lab.mount_path("index").join(name))?.read_exact_at(&mut got, READ_AT)?;
    ensure!(got[0] == bytes[READ_AT as usize], "probe read changed data");
    let bound = if zip {
        index_bytes(&version)
    } else {
        fallback_bytes(&version)
    };
    Ok((bound, wait_fetched(lab, name, bound)?))
}

fn unknown() -> Vec<u8> {
    let mut bytes = random_bytes(FILE_LEN);
    bytes[..16].fill(0x5a);
    bytes[FILE_LEN - 65_557..].fill(0x5a);
    bytes
}

fn invalid_zip() -> Vec<u8> {
    let mut bytes = unknown();
    let entry_len = ENTRY_HEADER + ENTRY_COMMENT;
    for index in 0..usize::from(ENTRIES) {
        let start = CENTRAL_START + index * entry_len;
        bytes[start..start + ENTRY_HEADER].fill(0);
        bytes[start..start + 4].copy_from_slice(b"PK\x01\x02");
        bytes[start + 32..start + 34].copy_from_slice(&(ENTRY_COMMENT as u16).to_le_bytes());
        bytes[start + 42..start + 46].copy_from_slice(&BODY_TARGET.to_le_bytes());
    }
    let end = bytes.len();
    bytes[end - 22..].fill(0);
    bytes[end - 22..end - 18].copy_from_slice(b"PK\x05\x06");
    bytes[end - 14..end - 12].copy_from_slice(&ENTRIES.to_le_bytes());
    bytes[end - 12..end - 10].copy_from_slice(&ENTRIES.to_le_bytes());
    bytes[end - 10..end - 6].copy_from_slice(&(CENTRAL_LEN as u32).to_le_bytes());
    bytes[end - 6..end - 2].copy_from_slice(&(CENTRAL_START as u32).to_le_bytes());
    bytes
}

/// An unknown file controls three ZIP-shaped files whose central-directory entries all point at
/// non-ZIP body bytes. Prefetch includes the bounded directory but never those referenced bytes.
// contract: P3, C9
#[test]
fn round67_zip_entries_do_not_widen_prefetch_beyond_the_directory() -> Result<()> {
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
    let (bound, fetched) = access(&lab, "zip-control.bin", &unknown(), false)?;
    ensure!(
        fetched == bound,
        "unknown control fetched {fetched}, expected {bound}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("missing-local-headers-{trial}.bin");
        let (bound, fetched) = access(&lab, &name, &invalid_zip(), true)?;
        eprintln!("trial {trial}: ZIP fetched {fetched}; index bound {bound}");
        failures += usize::from(fetched != bound);
    }
    assert_eq!(
        failures, 0,
        "ZIP indexes that fetched referenced body bytes, of 3"
    );
    Ok(())
}

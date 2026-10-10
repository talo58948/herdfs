//! Round 85: truncated MP4 file-type boxes must use bounded fallback prefetch.

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
const MOOV_START: usize = 16 << 20;
const MOOV_LEN: usize = 4 << 20;
const READ_AT: u64 = 8 << 20;
const WAIT: Duration = Duration::from_secs(5);

fn box_header(bytes: &mut [u8], start: usize, len: usize, kind: &[u8; 4]) {
    bytes[start..start + 4].copy_from_slice(&(len as u32).to_be_bytes());
    bytes[start + 4..start + 8].copy_from_slice(kind);
}

fn mp4(truncated_ftyp: bool) -> Vec<u8> {
    let mut bytes = random_bytes(FILE_LEN);
    let media = if truncated_ftyp {
        box_header(&mut bytes, 0, 8, b"ftyp");
        8
    } else {
        box_header(&mut bytes, 0, 16, b"ftyp");
        bytes[8..16].fill(0);
        16
    };
    box_header(&mut bytes, media, MOOV_START - media, b"mdat");
    box_header(&mut bytes, MOOV_START, MOOV_LEN, b"moov");
    box_header(&mut bytes, MOOV_START + 8, MOOV_LEN - 8, b"free");
    box_header(
        &mut bytes,
        MOOV_START + MOOV_LEN,
        FILE_LEN - MOOV_START - MOOV_LEN,
        b"mdat",
    );
    bytes
}

fn fetched_for(version: &Version, ranges: &[std::ops::Range<u64>]) -> u64 {
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

fn access(lab: &Lab, name: &str, truncated_ftyp: bool) -> Result<(u64, u64, u64)> {
    let bytes = mp4(truncated_ftyp);
    save(&lab.mount_path("writer").join(name), &bytes)?;
    let version = lab.head(ADMIN, name)?;
    thread::sleep(VISIBLE_AFTER);
    let mut got = [0];
    File::open(lab.mount_path("index").join(name))?.read_exact_at(&mut got, READ_AT)?;
    ensure!(got[0] == bytes[READ_AT as usize], "probe read changed data");
    let wanted = fetched_for(
        &version,
        &[
            0..32,
            READ_AT..READ_AT + 1,
            MOOV_START as u64..(MOOV_START + MOOV_LEN) as u64,
        ],
    );
    let fallback = fallback_bytes(&version);
    Ok((wanted, fallback, wait_fetched(lab, name, wanted)?))
}

/// A complete `ftyp` controls three files whose `ftyp` ends before its mandatory brand and version.
/// The truncated candidates are unexpected input and must use the edge-window fallback.
// contract: P3, C9
#[test]
#[ignore = "ROUND85: a truncated MP4 ftyp widens index prefetch"]
fn round85_truncated_mp4_ftyp_uses_fallback() -> Result<()> {
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
    let (wanted, fallback, fetched) = access(&lab, "complete-ftyp-control.mp4", false)?;
    ensure!(
        wanted != fallback,
        "control cannot distinguish index from fallback"
    );
    ensure!(fetched == wanted, "control fetched {fetched}, expected {wanted}");

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("truncated-ftyp-{trial}.bin");
        let (wanted, fallback, fetched) = access(&lab, &name, true)?;
        eprintln!(
            "trial {trial}: truncated ftyp fetched {fetched}; fallback {fallback}; claimed index {wanted}"
        );
        failures += usize::from(fetched > fallback);
    }
    assert_eq!(failures, 0, "truncated ftyp boxes widened prefetch, of 3");
    Ok(())
}

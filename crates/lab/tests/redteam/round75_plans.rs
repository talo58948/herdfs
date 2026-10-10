//! Round 75: hostile MP4 box walks must stay within the documented index-prefetch bound.

use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::proto::Version;
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, save};

const BOX_LEN: usize = 4 << 20;
const MAX_PREFETCH: u64 = 36 << 20;
const WAIT: Duration = Duration::from_secs(20);

fn box_header(bytes: &mut [u8], start: usize, len: usize, kind: &[u8; 4]) {
    bytes[start..start + 4].copy_from_slice(&(len as u32).to_be_bytes());
    bytes[start + 4..start + 8].copy_from_slice(kind);
}

fn valid_mp4() -> Vec<u8> {
    let mdat = 16;
    let moov = mdat + 20 * 1024 * 1024;
    let moov_len = 6 << 20;
    let mut bytes = vec![0; moov + moov_len];
    box_header(&mut bytes, 0, 16, b"ftyp");
    box_header(&mut bytes, mdat, moov - mdat, b"mdat");
    box_header(&mut bytes, moov, moov_len, b"moov");
    box_header(&mut bytes, moov + 8, moov_len - 8, b"free");
    bytes
}

fn hostile_mp4(seed: u8) -> Vec<u8> {
    let boxes = 12;
    let mut bytes = vec![0; 16 + boxes * BOX_LEN];
    box_header(&mut bytes, 0, 16, b"ftyp");
    for box_index in 0..boxes {
        let start = 16 + box_index * BOX_LEN;
        box_header(&mut bytes, start, BOX_LEN, b"mdat");
        bytes[start + 8] = seed.wrapping_mul(16).wrapping_add(box_index as u8 + 1);
    }
    let last = bytes.len() - 1;
    bytes[last] = seed.wrapping_add(1);
    bytes
}

fn fetched_for(version: &Version, ranges: &[std::ops::Range<u64>]) -> u64 {
    let mut ids = HashSet::new();
    let mut offset = 0;
    version
        .chunks
        .iter()
        .filter_map(|chunk| {
            let range = offset..offset + u64::from(chunk.len);
            offset = range.end;
            (ranges
                .iter()
                .any(|wanted| range.start < wanted.end && wanted.start < range.end)
                && ids.insert(chunk.id))
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

fn access(lab: &Lab, name: &str, bytes: &[u8], ranges: &[std::ops::Range<u64>]) -> Result<(u64, u64)> {
    save(&lab.mount_path("writer").join(name), bytes)?;
    let version = lab.head(ADMIN, name)?;
    thread::sleep(VISIBLE_AFTER);
    let mut got = [0];
    File::open(lab.mount_path("index").join(name))?.read_exact_at(&mut got, 0)?;
    ensure!(got[0] == bytes[0], "probe read changed data");
    let wanted = fetched_for(&version, ranges);
    Ok((wanted, wait_fetched(lab, name, wanted)?))
}

/// A valid late-moov file controls three malformed files containing a chain of bounded `mdat`
/// boxes. Header probes and fallback together must remain within the documented 36 MiB maximum.
// contract: C9, P3
#[test]
fn round75_mp4_probe_walk_stays_within_prefetch_bound() -> Result<()> {
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

    let control = valid_mp4();
    let moov = (16 + 20 * 1024 * 1024) as u64;
    let control_ranges = [0..16, (moov - 16)..moov, moov..control.len() as u64];
    let (wanted, fetched) = access(&lab, "probe-control.mp4", &control, &control_ranges)?;
    ensure!(
        fetched == wanted,
        "valid MP4 control fetched {fetched}, expected {wanted}"
    );
    ensure!(fetched <= MAX_PREFETCH, "valid MP4 control exceeded the bound");

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("probe-chain-{trial}.mp4");
        let bytes = hostile_mp4(trial);
        let mut ranges = Vec::with_capacity(13);
        ranges.push(0..16);
        ranges.extend((1..12).map(|index| {
            let start = (16 + index * BOX_LEN) as u64;
            start..start + 16
        }));
        let tail = bytes.len() as u64;
        ranges.push(tail - 65_557..tail);
        let (wanted, fetched) = access(&lab, &name, &bytes, &ranges)?;
        eprintln!("trial {trial}: MP4 probe chain fetched {fetched}; expected probe chunks {wanted}");
        failures += usize::from(fetched > MAX_PREFETCH);
    }
    assert_eq!(failures, 0, "MP4 probe walks over 36 MiB, of 3");
    Ok(())
}

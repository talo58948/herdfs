//! Round 73: bounded ZIP64 central directories are format indexes too.

use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::proto::Version;
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, random_bytes, save};

const PAYLOAD: usize = 8 << 20;
const READ_AT: u64 = 4 << 20;
const WAIT: Duration = Duration::from_secs(10);

fn lab() -> Result<Lab> {
    Lab::start(
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
    )
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

fn access(lab: &Lab, name: &str, bytes: &[u8], central: std::ops::Range<u64>) -> Result<(u64, u64, u64)> {
    save(&lab.mount_path("writer").join(name), bytes)?;
    let version = lab.head(ADMIN, name)?;
    thread::sleep(VISIBLE_AFTER);
    let mut got = [0];
    File::open(lab.mount_path("index").join(name))?.read_exact_at(&mut got, READ_AT)?;
    ensure!(got[0] == bytes[READ_AT as usize], "probe read changed data");
    let wanted = fetched_for(
        &version,
        &[0..16, READ_AT..READ_AT + 1, central.start..version.size],
    );
    let fallback = fallback_bytes(&version);
    ensure!(
        wanted > fallback,
        "{name} index is not distinguishable from fallback"
    );
    Ok((wanted, fallback, wait_fetched(lab, name, wanted)?))
}

fn archive(entries: u32, comment: usize, zip64: bool) -> (Vec<u8>, std::ops::Range<u64>) {
    let payload = random_bytes(PAYLOAD);
    let comments = random_bytes(entries as usize * comment);
    let mut bytes = Vec::with_capacity(PAYLOAD + entries as usize * (80 + comment));
    let mut offsets = Vec::with_capacity(entries as usize);
    for index in 0..entries {
        offsets.push(bytes.len() as u32);
        let name = index.to_le_bytes();
        let data = if index == 0 { payload.as_slice() } else { &[] };
        let mut header = [0; 30];
        header[..4].copy_from_slice(b"PK\x03\x04");
        header[4..6].copy_from_slice(&20_u16.to_le_bytes());
        header[14..18].copy_from_slice(&crc32(data).to_le_bytes());
        header[18..22].copy_from_slice(&(data.len() as u32).to_le_bytes());
        header[22..26].copy_from_slice(&(data.len() as u32).to_le_bytes());
        header[26..28].copy_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&name);
        bytes.extend_from_slice(data);
    }

    let central_start = bytes.len() as u64;
    for (index, offset) in offsets.into_iter().enumerate() {
        let name = (index as u32).to_le_bytes();
        let data = if index == 0 { payload.as_slice() } else { &[] };
        let mut header = [0; 46];
        header[..4].copy_from_slice(b"PK\x01\x02");
        header[4..6].copy_from_slice(&20_u16.to_le_bytes());
        header[6..8].copy_from_slice(&20_u16.to_le_bytes());
        header[16..20].copy_from_slice(&crc32(data).to_le_bytes());
        header[20..24].copy_from_slice(&(data.len() as u32).to_le_bytes());
        header[24..28].copy_from_slice(&(data.len() as u32).to_le_bytes());
        header[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
        header[32..34].copy_from_slice(&(comment as u16).to_le_bytes());
        header[42..46].copy_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&name);
        bytes.extend_from_slice(&comments[index * comment..(index + 1) * comment]);
    }
    let central_end = bytes.len() as u64;
    let central_len = central_end - central_start;

    if zip64 {
        let zip64_offset = bytes.len() as u64;
        bytes.extend_from_slice(b"PK\x06\x06");
        bytes.extend_from_slice(&44_u64.to_le_bytes());
        bytes.extend_from_slice(&45_u16.to_le_bytes());
        bytes.extend_from_slice(&45_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&u64::from(entries).to_le_bytes());
        bytes.extend_from_slice(&u64::from(entries).to_le_bytes());
        bytes.extend_from_slice(&central_len.to_le_bytes());
        bytes.extend_from_slice(&central_start.to_le_bytes());
        bytes.extend_from_slice(b"PK\x06\x07");
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&zip64_offset.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
    }

    bytes.extend_from_slice(b"PK\x05\x06");
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    let count = u16::try_from(entries).unwrap_or(u16::MAX);
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&(central_len as u32).to_le_bytes());
    bytes.extend_from_slice(&(central_start as u32).to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    (bytes, central_start..central_end)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// A bounded classic archive controls three valid ZIP64 archives whose only ZIP64 requirement is
/// their 65,536-entry count. Index prefetch must find each complete central directory.
// contract: P3, C9
#[test]
#[ignore = "outside P3: ZIP64 is not parsed; edge windows apply"]
fn round73_zip64_central_directory_is_prefetched() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = lab()?;
    let (control, central) = archive(112, u16::MAX as usize, false);
    let (wanted, _, fetched) = access(&lab, "classic-control.zip", &control, central)?;
    ensure!(
        fetched == wanted,
        "classic ZIP control fetched {fetched}, expected {wanted}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("zip64-{trial}.zip");
        let (bytes, central) = archive(65_536, 64, true);
        let (wanted, fallback, fetched) = access(&lab, &name, &bytes, central)?;
        eprintln!("trial {trial}: ZIP64 fetched {fetched}; fallback {fallback}; index requires {wanted}");
        failures += usize::from(fetched < wanted);
    }
    assert_eq!(failures, 0, "ZIP64 indexes missed, of 3");
    Ok(())
}

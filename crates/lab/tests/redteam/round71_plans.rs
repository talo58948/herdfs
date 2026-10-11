//! Round 71: valid indexes behind ambiguous or longer format structures.

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
const WAIT: Duration = Duration::from_secs(20);

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

fn access(lab: &Lab, name: &str, bytes: &[u8], wanted: impl FnOnce(&Version) -> u64) -> Result<(u64, u64)> {
    save(&lab.mount_path("writer").join(name), bytes)?;
    let version = lab.head(ADMIN, name)?;
    thread::sleep(VISIBLE_AFTER);
    let mut got = [0];
    File::open(lab.mount_path("index").join(name))?.read_exact_at(&mut got, READ_AT)?;
    ensure!(got[0] == bytes[READ_AT as usize], "probe read changed data");
    let wanted = wanted(&version);
    Ok((wanted, wait_fetched(lab, name, wanted)?))
}

const ZIP_ENTRIES: u16 = 112;
const ZIP_HEADER: usize = 46;
const ZIP_ENTRY_COMMENT: usize = u16::MAX as usize;
const ZIP_COMMENT: usize = 64;
const ZIP_DIRECTORY: usize = ZIP_ENTRIES as usize * (ZIP_HEADER + ZIP_ENTRY_COMMENT);
const ZIP_START: usize = FILE_LEN - ZIP_COMMENT - 22 - ZIP_DIRECTORY;

fn zip(comment_marker: bool) -> Vec<u8> {
    let mut bytes = random_bytes(FILE_LEN);
    bytes[..16].fill(0x5a);
    let entry_len = ZIP_HEADER + ZIP_ENTRY_COMMENT;
    for index in 0..usize::from(ZIP_ENTRIES) {
        let start = ZIP_START + index * entry_len;
        bytes[start..start + ZIP_HEADER].fill(0);
        bytes[start..start + 4].copy_from_slice(b"PK\x01\x02");
        bytes[start + 32..start + 34].copy_from_slice(&(ZIP_ENTRY_COMMENT as u16).to_le_bytes());
    }
    let eocd = FILE_LEN - ZIP_COMMENT - 22;
    bytes[eocd..].fill(0);
    bytes[eocd..eocd + 4].copy_from_slice(b"PK\x05\x06");
    bytes[eocd + 8..eocd + 10].copy_from_slice(&ZIP_ENTRIES.to_le_bytes());
    bytes[eocd + 10..eocd + 12].copy_from_slice(&ZIP_ENTRIES.to_le_bytes());
    bytes[eocd + 12..eocd + 16].copy_from_slice(&(ZIP_DIRECTORY as u32).to_le_bytes());
    bytes[eocd + 16..eocd + 20].copy_from_slice(&(ZIP_START as u32).to_le_bytes());
    bytes[eocd + 20..eocd + 22].copy_from_slice(&(ZIP_COMMENT as u16).to_le_bytes());
    if comment_marker {
        bytes[eocd + 30..eocd + 34].copy_from_slice(b"PK\x05\x06");
    }
    bytes
}

fn zip_bytes(version: &Version) -> u64 {
    fetched_for(
        version,
        &[0..16, READ_AT..READ_AT + 1, ZIP_START as u64..version.size],
    )
}

/// A normal archive comment controls three valid ZIP comments containing EOCD-shaped bytes. The
/// last signature in a comment must not hide the real central directory from index discovery.
// contract: P3, C9
#[test]
fn round71_zip_comment_signature_does_not_hide_the_index() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = lab()?;
    let (wanted, fetched) = access(&lab, "zip-control.bin", &zip(false), zip_bytes)?;
    ensure!(
        fetched == wanted,
        "ZIP control fetched {fetched}, expected {wanted}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("comment-signature-{trial}.zip");
        let (index, fetched) = access(&lab, &name, &zip(true), zip_bytes)?;
        let fallback = fallback_bytes(&lab.head(ADMIN, &name)?);
        eprintln!("trial {trial}: ZIP fetched {fetched}; fallback {fallback}; index requires {index}");
        failures += usize::from(fetched < index);
    }
    assert_eq!(failures, 0, "valid ZIP indexes missed, of 3");
    Ok(())
}

const MOOV_LEN: usize = 6 << 20;
const MDAT_LEN: usize = 20 << 20;

fn box_header(bytes: &mut [u8], start: usize, len: usize, kind: &[u8; 4]) {
    bytes[start..start + 4].copy_from_slice(&(len as u32).to_be_bytes());
    bytes[start + 4..start + 8].copy_from_slice(kind);
}

fn mp4(extra_box: bool) -> (Vec<u8>, std::ops::Range<u64>) {
    let small_boxes = if extra_box { 3 } else { 2 };
    let prefix = small_boxes * 16;
    let mdat = prefix;
    let moov = mdat + MDAT_LEN;
    let mut bytes = random_bytes(moov + MOOV_LEN);
    box_header(&mut bytes, 0, 16, b"ftyp");
    box_header(&mut bytes, 16, 16, b"free");
    if extra_box {
        box_header(&mut bytes, 32, 16, b"free");
    }
    box_header(&mut bytes, mdat, MDAT_LEN, b"mdat");
    box_header(&mut bytes, moov, MOOV_LEN, b"moov");
    box_header(&mut bytes, moov + 8, MOOV_LEN - 8, b"mvhd");
    (bytes, moov as u64..(moov + MOOV_LEN) as u64)
}

/// A three-box prefix controls three MP4s with one additional harmless top-level box. Both place
/// a bounded `moov` after `mdat`, so index discovery must not depend on a four-box probe limit.
// contract: P3, C9
#[test]
fn round71_mp4_fifth_box_does_not_hide_the_index() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = lab()?;
    let (control, moov) = mp4(false);
    let (wanted, fetched) = access(&lab, "mp4-control.mp4", &control, |version| {
        fetched_for(version, &[0..16, READ_AT..READ_AT + 1, moov.clone()])
    })?;
    ensure!(
        fetched == wanted,
        "MP4 control fetched {fetched}, expected {wanted}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("fifth-box-{trial}.mp4");
        let (bytes, moov) = mp4(true);
        let (index, fetched) = access(&lab, &name, &bytes, |version| {
            fetched_for(version, &[0..16, READ_AT..READ_AT + 1, moov])
        })?;
        let fallback = fallback_bytes(&lab.head(ADMIN, &name)?);
        eprintln!("trial {trial}: MP4 fetched {fetched}; fallback {fallback}; index requires {index}");
        failures += usize::from(fetched < index);
    }
    assert_eq!(failures, 0, "valid MP4 indexes missed, of 3");
    Ok(())
}

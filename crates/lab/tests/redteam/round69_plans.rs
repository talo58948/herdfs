//! Round 69: MP4 index discovery after a large media-data box.

use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::proto::Version;
use herdfs_lab::{ADMIN, JevMode, Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, random_bytes, save};

const MOOV_LEN: usize = 6 << 20;
const MDAT_LEN: usize = 20 << 20;
const WAIT: Duration = Duration::from_secs(20);

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

fn mp4(late: bool) -> (Vec<u8>, std::ops::Range<u64>) {
    let ftyp = 16;
    let (mdat_start, moov_start) = if late {
        (ftyp, ftyp + MDAT_LEN)
    } else {
        (ftyp + MOOV_LEN, ftyp)
    };
    let mut bytes = random_bytes(ftyp + MDAT_LEN + MOOV_LEN);
    box_header(&mut bytes, 0, ftyp, b"ftyp");
    box_header(&mut bytes, mdat_start, MDAT_LEN, b"mdat");
    box_header(&mut bytes, moov_start, MOOV_LEN, b"moov");
    box_header(&mut bytes, moov_start + 8, MOOV_LEN - 8, b"free");
    (bytes, moov_start as u64..(moov_start + MOOV_LEN) as u64)
}

fn box_header(bytes: &mut [u8], start: usize, len: usize, kind: &[u8; 4]) {
    bytes[start..start + 4].copy_from_slice(&(len as u32).to_be_bytes());
    bytes[start + 4..start + 8].copy_from_slice(kind);
}

fn access(lab: &Lab, name: &str, late: bool) -> Result<(u64, u64)> {
    let (bytes, moov) = mp4(late);
    save(&lab.mount_path("writer").join(name), &bytes)?;
    let version = lab.head(ADMIN, name)?;
    thread::sleep(VISIBLE_AFTER);
    let read = (MDAT_LEN / 2) as u64;
    let mut got = [0];
    File::open(lab.mount_path("index").join(name))?.read_exact_at(&mut got, read)?;
    ensure!(got[0] == bytes[read as usize], "probe read changed data");
    let wanted = fetched_for(&version, &[0..16, read..read + 1, moov]);
    let settled = if late {
        fallback_bytes(&version, read)
    } else {
        wanted
    };
    Ok((wanted, wait_fetched(lab, name, settled)?))
}

/// An early-moov control and three ordinary large-mdat/late-moov files must all prefetch the
/// complete moov index named by their valid top-level box structure.
// contract: P3, C9
#[test]
#[ignore = "ROUND69: MP4 index after a large mdat box is not fully prefetched"]
fn round69_mp4_index_after_large_mdat_is_prefetched() -> Result<()> {
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
    let (wanted, fetched) = access(&lab, "early-moov-control.mp4", false)?;
    ensure!(
        fetched == wanted,
        "early-moov control fetched {fetched}, expected {wanted}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("late-moov-{trial}.mp4");
        let (wanted, fetched) = access(&lab, &name, true)?;
        eprintln!("trial {trial}: late MP4 fetched {fetched}; index requires {wanted}");
        failures += usize::from(fetched != wanted);
    }
    assert_eq!(failures, 0, "late MP4 indexes not fully prefetched, of 3");
    Ok(())
}

fn wait_answers(lab: &Lab, wanted: u64) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        if lab.stats("jev")?["jev_answers"]
            .as_u64()
            .is_some_and(|answers| answers >= wanted)
        {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "Jev answers did not reach {wanted}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// In each of three matched pairs, the first open controls that a delayed Jev answer upgrades a
/// live access. The simultaneous second access must receive the same additive upgrade.
// contract: P1
#[test]
#[ignore = "ROUND69: an in-flight Jev answer upgrades only its first matching access"]
fn round69_inflight_jev_answer_upgrades_every_matching_access() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("writer", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("jev", "jev", &[], Profile::Lan, Strategy::Jev)
            .on_tmpfs(),
    )?;
    lab.jev("jev").set(JevMode::Slow(Duration::from_millis(750)));

    let mut failures = 0;
    for trial in 0..3 {
        let first = format!("matched-first-{trial}.pair{trial}");
        let second = format!("matched-second-{trial}.pair{trial}");
        save(&lab.mount_path("writer").join(&first), &random_bytes(64 << 20))?;
        save(&lab.mount_path("writer").join(&second), &random_bytes(64 << 20))?;
        lab.wait_committed("writer")?;
        thread::sleep(VISIBLE_AFTER);

        let first_open = File::open(lab.mount_path("jev").join(&first))?;
        let second_open = File::open(lab.mount_path("jev").join(&second))?;
        wait_answers(&lab, (trial + 1) * 3)?;
        ensure!(
            lab.jev("jev").requests() == trial as usize + 1,
            "matched accesses did not share one Jev request"
        );
        let control = lab.plan("jev", first.as_ref())?;
        let observed = lab.plan("jev", second.as_ref())?;
        ensure!(
            control["source"] == "jev" && control["plan"]["prefetch"] == "whole",
            "first-access control was not upgraded: {control}"
        );
        let upgraded = observed["source"] == "jev" && observed["plan"]["prefetch"] == "whole";
        eprintln!(
            "trial {trial}: first source={}, second source={}, second prefetch={}",
            control["source"], observed["source"], observed["plan"]["prefetch"]
        );
        failures += usize::from(!upgraded);
        drop((first_open, second_open));
    }
    assert_eq!(
        failures, 0,
        "matching live accesses that missed the late answer, of 3"
    );
    Ok(())
}

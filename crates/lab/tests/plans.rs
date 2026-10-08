//! Access-plan C9 bounds through real FUSE reads and the stats xattr.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::proto::Version;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

const FILE: &str = "unknown.bin";
const FILE_LEN: usize = 24 << 20;
const READ: usize = 64 << 10;
const FIRST_CHUNKS: usize = 2;
const SMALL_CHUNKS: usize = 2;
const LARGE_CHUNKS: usize = 8;
const WAIT: Duration = Duration::from_secs(15);

#[test]
fn cold_accesses_fetch_only_their_plan_regions() -> Result<()> {
    // contract: P1, P3, C8, C9
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let plans = [
        ("stream", Strategy::Stream),
        ("fetch", Strategy::Fetch),
        ("pin", Strategy::Pin),
        ("first", Strategy::Plan("first/none/no")),
        ("index", Strategy::Plan("index/none/no")),
        ("small", Strategy::Plan("none/small/no")),
        ("large", Strategy::Plan("none/large/no")),
    ];
    let config = plans.iter().fold(
        LabConfig::default().client("writer", "alice", &[], Profile::Lan, Strategy::Stream),
        |config, &(name, plan)| config.client(name, name, &[], Profile::Lan, plan),
    );
    let lab = Lab::start(config.on_tmpfs())?;
    let mut bytes = vec![0; FILE_LEN];
    rand::fill(&mut bytes[..]);
    let mut written = File::create(lab.mount_path("writer").join(FILE))?;
    written.write_all(&bytes)?;
    written.sync_all()?;
    let version = lab.head("alice", FILE)?;
    thread::sleep(Duration::from_millis(1_200));

    let middle = version.size / 2;
    for name in ["stream", "fetch", "pin", "first", "index"] {
        let file = File::open(lab.mount_path(name).join(FILE))?;
        let mut byte = [0];
        file.read_exact_at(&mut byte, middle)?;
        assert_eq!(byte[0], bytes[middle as usize]);
        let wanted = planned(&version, name, middle, 1);
        assert_fetches(&lab, name, wanted)?;
    }
    for (name, ahead) in [("small", SMALL_CHUNKS), ("large", LARGE_CHUNKS)] {
        let file = File::open(lab.mount_path(name).join(FILE))?;
        let start = middle;
        let mut got = vec![0; READ * 2];
        file.read_exact_at(&mut got[..READ], start)?;
        file.read_exact_at(&mut got[READ..], start + READ as u64)?;
        assert_eq!(got, bytes[start as usize..start as usize + got.len()]);
        let wanted = sequential(&version, start, got.len() as u64, ahead);
        assert_fetches(&lab, name, wanted)?;
    }
    Ok(())
}

fn planned(version: &Version, name: &str, off: u64, len: u64) -> u64 {
    let mut indices = overlapping(version, off, len);
    match name {
        "fetch" | "pin" => indices.extend(0..version.chunks.len()),
        "first" => indices.extend(0..version.chunks.len().min(FIRST_CHUNKS)),
        "index" => {
            indices.extend(0..version.chunks.len().min(FIRST_CHUNKS));
            indices.extend(version.chunks.len().saturating_sub(FIRST_CHUNKS)..version.chunks.len());
        }
        "stream" => {}
        _ => unreachable!(),
    }
    chunk_bytes(version, &indices)
}

fn sequential(version: &Version, off: u64, len: u64, ahead: usize) -> u64 {
    let mut indices = overlapping(version, off, len);
    let next = indices.last().map_or(0, |index| index + 1);
    indices.extend(next..next.saturating_add(ahead).min(version.chunks.len()));
    chunk_bytes(version, &indices)
}

fn overlapping(version: &Version, off: u64, len: u64) -> BTreeSet<usize> {
    let end = off.saturating_add(len).min(version.size);
    let mut start = 0;
    version
        .chunks
        .iter()
        .enumerate()
        .filter_map(|(index, chunk)| {
            let chunk_end = start + u64::from(chunk.len);
            let overlaps = start < end && off < chunk_end;
            start = chunk_end;
            overlaps.then_some(index)
        })
        .collect()
}

fn chunk_bytes(version: &Version, indices: &BTreeSet<usize>) -> u64 {
    indices
        .iter()
        .map(|&index| u64::from(version.chunks[index].len))
        .sum()
}

fn assert_fetches(lab: &Lab, client: &str, wanted: u64) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let fetched = lab.stats(client)?["bytes_fetched"]
            .as_u64()
            .context("stats lack bytes_fetched")?;
        ensure!(
            fetched <= wanted,
            "{client} fetched {fetched} bytes, beyond its {wanted}-byte plan"
        );
        if fetched == wanted {
            let plan = lab.plan(client, FILE.as_ref())?;
            assert_eq!(plan["bytes_fetched"], fetched);
            assert_eq!(plan["source"], "fixed");
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "{client} fetched {fetched}, expected {wanted}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

//! Round 81: MP4 index discovery across a valid large padding box.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::proto::Version;
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, random_bytes, save};

const GAP_LEN: usize = 12 << 20;
const MOOV_LEN: usize = 2 << 20;
const TRAIL_LEN: usize = 8 << 20;
const WAIT: Duration = Duration::from_secs(10);

fn box_header(bytes: &mut [u8], start: usize, len: usize, kind: &[u8; 4]) {
    bytes[start..start + 4].copy_from_slice(&(len as u32).to_be_bytes());
    bytes[start + 4..start + 8].copy_from_slice(kind);
}

fn mp4(gap_kind: &[u8; 4]) -> (Vec<u8>, std::ops::Range<u64>) {
    let ftyp = 16;
    let gap = ftyp;
    let moov = gap + GAP_LEN;
    let trail = moov + MOOV_LEN;
    let mut bytes = random_bytes(trail + TRAIL_LEN);
    box_header(&mut bytes, 0, ftyp, b"ftyp");
    box_header(&mut bytes, gap, GAP_LEN, gap_kind);
    box_header(&mut bytes, moov, MOOV_LEN, b"moov");
    box_header(&mut bytes, moov + 8, MOOV_LEN - 8, b"free");
    box_header(&mut bytes, trail, TRAIL_LEN, b"mdat");
    (bytes, moov as u64..trail as u64)
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

fn access(lab: &Lab, name: &str, gap_kind: &[u8; 4]) -> Result<(u64, u64)> {
    let (bytes, moov) = mp4(gap_kind);
    save(&lab.mount_path("writer").join(name), &bytes)?;
    let version = lab.head(ADMIN, name)?;
    thread::sleep(VISIBLE_AFTER);
    let mut got = [0];
    File::open(lab.mount_path("index").join(name))?.read_exact_at(&mut got, 0)?;
    ensure!(got[0] == bytes[0], "probe read changed data");
    let wanted = fetched_for(&version, &[0..16, moov]);
    Ok((wanted, wait_fetched(lab, name, wanted)?))
}

/// A large `mdat` controls three equally sized valid `free` padding boxes. Both may precede the
/// `moov`, so index discovery must skip either box and prefetch the complete movie index.
// contract: P3, C9
#[test]
fn round81_mp4_index_after_large_free_box_is_prefetched() -> Result<()> {
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
    let (wanted, fetched) = access(&lab, "mdat-control.mp4", b"mdat")?;
    ensure!(
        fetched == wanted,
        "mdat control fetched {fetched}, expected {wanted}"
    );

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("free-before-moov-{trial}.mp4");
        let (wanted, fetched) = access(&lab, &name, b"free")?;
        eprintln!("trial {trial}: padded MP4 fetched {fetched}; index requires {wanted}");
        failures += usize::from(fetched != wanted);
    }
    assert_eq!(failures, 0, "padded MP4 indexes not fully prefetched, of 3");
    Ok(())
}

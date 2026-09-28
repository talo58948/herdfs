//! C15 and C14: the store holds what retention keeps, every kept version reads back, and an upload
//! waiting for its commit is not collected under it.

use std::collections::BTreeMap;
use std::thread;
use std::time::Instant;

use anyhow::Result;
use herdfs_core::proto::ErrorCode;
use herdfs_lab::SPACE;

use crate::{
    ALICE, ERASED_WITHIN, QUICK, SECRET_LEN, commit, create, history, random_bytes, read_version, start,
    stored, upload, wait_until, write,
};

/// Versions written in the retention test, far more than it keeps.
const WRITES: usize = 20;
const VERSION_LEN: usize = 512 << 10;

// contract: C15, C14
#[test]
fn retention_bounds_the_store_to_the_versions_it_keeps() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let mut written = Vec::new();
    for _ in 0..WRITES {
        let bytes = random_bytes(VERSION_LEN);
        written.push((write(&lab, "log.bin", &bytes)?, bytes));
    }
    // The head is the newest of the last N versions.
    let kept = &written[WRITES - QUICK.retain_versions as usize..];
    let live: BTreeMap<String, u64> = (kept.iter())
        .flat_map(|(version, _)| &version.chunks)
        .map(|chunk| (format!("chunks/{}", chunk.id), u64::from(chunk.len)))
        .collect();
    // Pruned versions' chunks may stay until their uploads are older than the grace.
    wait_until("the store holds only retained chunks", ERASED_WITHIN, || {
        Ok(stored(&lab)? == live)
    })?;
    let bytes: u64 = live.values().sum();
    eprintln!(
        "{WRITES} versions written; {} objects, {bytes} bytes stored",
        live.len()
    );
    let versions: Vec<_> = kept.iter().map(|(version, _)| version.clone()).collect();
    assert_eq!(history(&lab, ALICE, "log.bin")?, versions);
    for (version, bytes) in kept {
        let read = read_version(&lab, ALICE, SPACE, version)?;
        assert!(read == *bytes, "version {} reads back other bytes", version.id.0);
    }
    Ok(())
}

/// A slow client: its uploads wait unreferenced while the collector runs every second, and its
/// commit comes just inside the grace.
// contract: C15, C14
#[test]
fn an_upload_committed_within_the_grace_survives_collection() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let bytes = random_bytes(SECRET_LEN);
    let entry = create(&lab, ALICE, SPACE, "slow.bin")?;
    // A saturated scheduler may miss the setup window; only exercise the contract once it did not.
    let chunks = loop {
        let started = Instant::now();
        let chunks = upload(&lab, ALICE, SPACE, &bytes)?;
        let commit_at = started + QUICK.grace - 2 * QUICK.every;
        thread::sleep(commit_at.saturating_duration_since(Instant::now()));
        if started.elapsed() < QUICK.grace {
            break chunks;
        }
    };
    let version = commit(&lab, ALICE, SPACE, entry, chunks)??;
    assert!(read_version(&lab, ALICE, SPACE, &version)? == bytes);
    thread::sleep(ERASED_WITHIN);
    assert!(read_version(&lab, ALICE, SPACE, &version)? == bytes);
    Ok(())
}

/// Past the grace, the collector may erase an upload nothing references. The commit that comes
/// then may be refused, but a version it makes must never lack its bytes.
// contract: C14
#[test]
fn a_commit_after_the_grace_never_names_erased_bytes() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let bytes = random_bytes(SECRET_LEN);
    let entry = create(&lab, ALICE, SPACE, "late.bin")?;
    let chunks = upload(&lab, ALICE, SPACE, &bytes)?;
    thread::sleep(ERASED_WITHIN);
    match commit(&lab, ALICE, SPACE, entry, chunks)? {
        Ok(version) => {
            let read = read_version(&lab, ALICE, SPACE, &version)?;
            assert!(read == bytes, "a committed version lost its bytes");
        }
        Err(refusal) => assert_eq!(refusal.code, ErrorCode::Invalid, "{refusal}"),
    }
    Ok(())
}

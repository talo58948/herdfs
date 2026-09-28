//! S6: commits of chunks bob never uploaded, and uploads as an oracle for what the store holds.

use std::time::Instant;

use anyhow::{Result, bail};
use herdfs_core::proto::{Request, Response};
use herdfs_core::{ChunkId, ChunkRef, SpacePath};
use herdfs_lab::SPACE;
use reqwest::StatusCode;
use serde_json::json;

use crate::{ALICE, Scene, commit, random_bytes, write_as_alice};

/// A file of alice's with enough chunks to time an upload of each against a fresh one.
const ARCHIVE_LEN: usize = 24 << 20;
/// Paired upload times may differ by this much in the median, or by a quarter of the fresh
/// median if that is more.
const TIMING_SLACK_MS: f64 = 15.0;

// contract: S6
#[test]
fn bob_cannot_commit_alices_chunks_without_uploading_them() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    let (secret, plan) = (&scene.secret, &scene.plan);
    let create = json!({"op": "create", "space": SPACE, "path": "eng/claim", "mode": 0o644});
    let created = scene.attacker.rpc(&create)?;
    let Response::Entry { entry: claim } = serde_json::from_slice(&created.body)? else {
        bail!("create answers with the entry");
    };
    let mut padded = plan.version.chunks.clone();
    padded.push(secret.version.chunks[0]);
    // A base bob cannot use (alice's, on another entry) answers as a missing base does (S8).
    let claims = [
        (
            commit(claim.id, None, secret.version.chunks.clone()),
            StatusCode::BAD_REQUEST,
            "invalid",
        ),
        (
            commit(claim.id, Some(secret.version.id), secret.version.chunks.clone()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            commit(plan.version.entry, Some(plan.version.id), padded),
            StatusCode::BAD_REQUEST,
            "invalid",
        ),
    ];
    for (request, status, code) in &claims {
        let reply = scene.attacker.call(request)?;
        assert_eq!(reply.status, *status, "{request:?}");
        assert_eq!(reply.json()?["code"], *code);
    }
    // The refusal reads the same whether or not the chunk is stored anywhere.
    let stored = secret.version.chunks[0];
    let absent = ChunkRef {
        id: ChunkId::of(&random_bytes(stored.len as usize)),
        len: stored.len,
    };
    let refusal = |chunk: ChunkRef| -> Result<(StatusCode, String)> {
        let reply = scene.attacker.call(&commit(claim.id, None, vec![chunk]))?;
        let body = String::from_utf8(reply.body)?.replace(&chunk.id.to_string(), "<id>");
        Ok((reply.status, body))
    };
    assert_eq!(refusal(stored)?, refusal(absent)?);
    let stat = Request::Stat {
        space: SPACE.into(),
        path: SpacePath::try_from("eng/claim")?,
    };
    let Response::Entry { entry } = scene.lab.rpc(ALICE, &stat)? else {
        bail!("stat answers with the entry");
    };
    assert_eq!(entry.head, None, "nothing was committed");
    scene.assert_no_leak()
}

// contract: S6
#[test]
fn an_upload_answers_the_same_whether_or_not_the_store_has_the_bytes() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    // Each of alice's chunks can be tested once, as a guess would be; after that it is stored
    // whatever the answer. So bob uploads each once, paired with fresh bytes of its length.
    let archive = write_as_alice(&scene.lab, "finance/archive.bin", random_bytes(ARCHIVE_LEN))?;
    let (mut gaps_ms, mut fresh_ms) = (Vec::new(), Vec::new());
    for (index, stored) in archive.version.chunks.iter().enumerate() {
        let bytes = random_bytes(stored.len as usize);
        let fresh = ChunkRef {
            id: ChunkId::of(&bytes),
            len: stored.len,
        };
        let pair = [(stored, archive.chunk(index)), (&fresh, &bytes[..])];
        let mut elapsed_ms = [0.0; 2];
        // Alternating which goes first keeps any warm-up from favouring one.
        for which in [index % 2, 1 - index % 2] {
            let (chunk, bytes) = pair[which];
            let start = Instant::now();
            let reply = scene.attacker.upload(Some(&scene.attacker.token), chunk, bytes)?;
            elapsed_ms[which] = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(reply.status, StatusCode::OK);
            assert_eq!(reply.json()?, json!({"type": "done"}));
        }
        gaps_ms.push(elapsed_ms[0] - elapsed_ms[1]);
        fresh_ms.push(elapsed_ms[1]);
    }
    let slower = gaps_ms.iter().filter(|gap| **gap > 0.0).count();
    let (gap, fresh) = (median(gaps_ms), median(fresh_ms));
    eprintln!(
        "{} pairs: fresh median {fresh:.1} ms, stored slower in {slower}, median gap {gap:.1} ms",
        archive.version.chunks.len()
    );
    // Loose on purpose, so the store's jitter cannot fail it: this catches an upload that skips
    // the store write, which saves most of its time, but not the few milliseconds the store may
    // spend overwriting an object, which the line above prints.
    assert!(
        gap.abs() <= TIMING_SLACK_MS.max(fresh / 4.0),
        "median gap {gap:.1} ms on {fresh:.1} ms"
    );
    scene.assert_no_leak()
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

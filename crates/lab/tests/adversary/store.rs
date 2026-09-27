//! Attacks on the store and on grants: requests without a grant, store keys on bob's host,
//! edited and outlived grant URLs, revocation, and a tampered cache.

use std::fs;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use herdfs_core::ChunkId;
use herdfs_core::proto::{Request, Response};
use herdfs_lab::{SPACE, STORE_CREDENTIALS, sweep};
use reqwest::StatusCode;
use serde_json::json;

use crate::{ALICE, BOB, EACCES, Reply, Scene, assert_same, contains, errno, set_acl};

/// The grant TTL of the contract.
const GRANT_TTL_MS: i64 = 5 * 60 * 1000;

fn grants(reply: &Reply) -> Result<(Vec<String>, i64)> {
    match serde_json::from_slice(&reply.body)? {
        Response::Grants { urls, expires_ms } => Ok((urls, expires_ms)),
        other => bail!("{}: expected grants, got {other:?}", reply.status),
    }
}

fn now_ms() -> i64 {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

/// bob's request for every chunk of the plan.
fn plan_grant(scene: &Scene) -> Request {
    Request::Grant {
        space: SPACE.into(),
        path: scene.plan.path.clone(),
        version: scene.plan.version.id,
        chunks: scene.plan.chunk_indices(),
    }
}

// contract: S1
#[test]
fn the_store_refuses_requests_without_a_grant() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    // A chunk's key is its hash, so anyone who guesses the bytes knows the key.
    let (small, secret) = (&scene.small_secret, &scene.secret);
    assert_eq!(ChunkId::of(&small.bytes), small.version.chunks[0].id);
    for chunk in secret.version.chunks.iter().chain(&small.version.chunks) {
        let mut url = scene.lab.presign_get(&format!("chunks/{}", chunk.id));
        url.set_query(None);
        assert_eq!(
            scene.attacker.get(url.as_str())?.status,
            StatusCode::FORBIDDEN,
            "{url}"
        );
    }
    let mut listing = scene.lab.presign_get("chunks");
    listing.set_query(Some("list-type=2&prefix=chunks/"));
    listing.set_path(&listing.path().replace("/chunks", "/"));
    let listed = scene.attacker.get(listing.as_str())?;
    assert_eq!(listed.status, StatusCode::FORBIDDEN, "{listing}");
    scene.assert_no_leak()
}

// contract: S1
#[test]
fn bob_holds_no_store_credentials() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    // The access key id is left out: every presigned URL carries it, by design of S3.
    let [(access_name, _), (secret_name, secret)] = STORE_CREDENTIALS;
    let credentials = [access_name, secret_name, secret].map(String::from);
    assert_eq!(sweep(&[scene.lab.client_dir(BOB)], &credentials)?, []);
    let pid = scene.lab.client_pid(BOB).context("bob's mount runs")?;
    for file in ["environ", "cmdline"] {
        let text = fs::read(format!("/proc/{pid}/{file}"))?;
        let held = |credential: &&String| contains(&text, credential.as_bytes());
        let found: Vec<&String> = credentials.iter().filter(held).collect();
        assert!(found.is_empty(), "bob's mount has {found:?} in its {file}");
    }
    Ok(())
}

// contract: S2
#[test]
fn an_edited_grant_url_is_refused() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    let plan = &scene.plan;
    let (urls, _) = grants(&scene.attacker.call(&plan_grant(&scene))?)?;
    let genuine = scene.attacker.get(&urls[0])?;
    assert_eq!(genuine.status, StatusCode::OK);
    assert_same(&genuine.body, plan.chunk(0), "the genuine URL");
    let own = plan.version.chunks[0].id.to_string();
    let edits = [
        urls[0].replace(&own, &scene.secret.version.chunks[0].id.to_string()),
        urls[0].replace(&own, &plan.version.chunks[1].id.to_string()),
        urls[0].replace("X-Amz-Expires=300", "X-Amz-Expires=604800"),
    ];
    for edited in &edits {
        assert_ne!(edited, &urls[0], "the edit applies");
        assert_eq!(
            scene.attacker.get(edited)?.status,
            StatusCode::FORBIDDEN,
            "{edited}"
        );
    }
    scene.assert_no_leak()
}

// contract: S4
#[test]
fn revocation_stops_new_grants_at_once() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    let (urls, expires_ms) = grants(&scene.attacker.call(&plan_grant(&scene))?)?;
    let granted_by = now_ms();
    set_acl(&scene.lab, ALICE, "eng", "user:bob", None)?;
    for _ in 0..5 {
        assert_eq!(
            scene.attacker.call(&plan_grant(&scene))?.status,
            StatusCode::FORBIDDEN
        );
    }
    let open = json!({"op": "open", "space": SPACE, "path": "eng/plan.txt"});
    assert_eq!(scene.attacker.rpc(&open)?.status, StatusCode::FORBIDDEN);
    assert_eq!(errno(fs::read(scene.bob_mount("eng/plan.txt"))), EACCES);
    // An outstanding URL stays good until it expires, within the TTL of its grant, as S4
    // allows; `an_outstanding_grant_url_dies_with_its_ttl` waits for that.
    assert!(expires_ms <= granted_by + GRANT_TTL_MS, "expires {expires_ms}");
    let outstanding = scene.attacker.get(&urls[0])?;
    assert_eq!(outstanding.status, StatusCode::OK);
    assert_same(&outstanding.body, scene.plan.chunk(0), "the outstanding URL");
    assert_eq!(scene.leaks(&[])?, [], "canaries reached bob");
    Ok(())
}

// contract: S2, S4
#[test]
#[ignore = "slow: waits out the 5-minute grant TTL, which the authority does not let tests shorten"]
fn an_outstanding_grant_url_dies_with_its_ttl() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    let (urls, expires_ms) = grants(&scene.attacker.call(&plan_grant(&scene))?)?;
    set_acl(&scene.lab, ALICE, "eng", "user:bob", None)?;
    assert_eq!(
        scene.attacker.get(&urls[0])?.status,
        StatusCode::OK,
        "good until it expires"
    );
    let left = u64::try_from(expires_ms - now_ms()).unwrap_or_default();
    thread::sleep(Duration::from_millis(left) + Duration::from_secs(2));
    for url in &urls {
        assert_eq!(scene.attacker.get(url)?.status, StatusCode::FORBIDDEN, "{url}");
    }
    assert_eq!(scene.leaks(&[])?, [], "canaries reached bob");
    Ok(())
}

/// The attacker forges bob's cache files before bob reads. A chunk the client already holds in
/// memory is not read from disk again until it restarts, and bob cannot restart in this scene
/// (see `bob_mounts_with_access_below_the_root_only`).
// contract: C11
#[test]
fn a_tampered_cached_chunk_is_refetched() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    let (plan, file) = (&scene.plan, scene.bob_mount("eng/plan.txt"));
    let cache = scene.lab.client_dir(BOB).join("cache");
    let cached = |index: usize| cache.join(plan.version.chunks[index].id.to_string());
    // Other bytes of the right length, a truncation, and a genuine chunk under another's name.
    fs::write(cached(0), vec![0; plan.chunk(0).len()])?;
    fs::write(cached(1), &plan.chunk(1)[..plan.chunk(1).len() / 2])?;
    fs::write(cached(2), plan.chunk(0))?;
    assert_same(&fs::read(&file)?, &plan.bytes, "the plan read over forged chunks");
    let stats = scene.lab.stats(BOB)?;
    let fetched = stats["chunks_fetched"]
        .as_u64()
        .context("stats count fetched chunks")?;
    let chunks = plan.version.chunks.len() as u64;
    assert!(
        fetched >= chunks,
        "{fetched} of {chunks} chunks fetched: a forged one was served"
    );
    for index in 0..3 {
        assert_same(
            &fs::read(cached(index))?,
            plan.chunk(index),
            "a chunk cached anew",
        );
    }
    scene.assert_no_leak()
}

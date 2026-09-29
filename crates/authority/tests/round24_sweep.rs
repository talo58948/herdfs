//! Red team, round 24: collection timestamps after failures and wall-clock jumps.
#![allow(clippy::unwrap_used, reason = "test code")]

mod http;

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::http::StatusCode;
use http::{Lab, now_ms};
use herdfs_authority::Policy;
use herdfs_core::ChunkId;
use serde_json::json;

const GRACE_MS: i64 = 1_000;
const INTERVAL_MS: i64 = 200;
const GRANT_TTL_MS: i64 = 20;
const POLICY: Policy = Policy {
    versions: 1,
    max_age_ms: 0,
    grace_ms: GRACE_MS,
};

fn key(bytes: &[u8]) -> String {
    format!("herdfs/chunks/{}", ChunkId::of(bytes))
}

async fn delete_chunk_space(lab: &Lab, bytes: &[u8]) {
    let alice = lab.user("alice");
    let root = lab.create_space(&alice, "team").await;
    let (_, file) = lab
        .rpc(
            &alice,
            &json!({"op": "create", "space": "team", "parent": root, "name": "f", "mode": 420}),
        )
        .await;
    let id = ChunkId::of(bytes);
    assert_eq!(
        lab.upload(&alice, "team", id, bytes.to_vec()).await.0,
        StatusCode::OK
    );
    let chunk = json!({"id": id.to_string(), "len": bytes.len()});
    let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"],
        "request_id": "r1", "base": null, "guarded": false, "chunks": [chunk]});
    assert_eq!(lab.rpc(&alice, &commit).await.0, StatusCode::OK);
    assert_eq!(
        lab.rpc(&alice, &json!({"op": "delete_space", "space": "team"}))
            .await
            .0,
        StatusCode::OK
    );
}

async fn lab_with_deleted_chunk(bytes: &[u8]) -> (Lab, i64) {
    let lab = Lab::start_with_ttl(Duration::from_millis(GRANT_TTL_MS as u64)).await;
    let started = now_ms();
    lab.server.sweep(started, &POLICY).await.unwrap();
    delete_chunk_space(&lab, bytes).await;
    (lab, now_ms())
}

/// Control: after a stall with no collection attempts, resumed interval collections erase the
/// chunk within the grant TTL plus one interval.
// contract: S7
#[tokio::test]
async fn a_resumed_collector_without_a_failed_pass_meets_the_recovery_bound() {
    let bytes = b"round 24 stalled control";
    let (lab, deleted_at) = lab_with_deleted_chunk(bytes).await;
    let resumed = deleted_at + GRACE_MS + INTERVAL_MS;

    lab.server.sweep(resumed, &POLICY).await.unwrap();
    lab.server
        .sweep(resumed + GRANT_TTL_MS + INTERVAL_MS, &POLICY)
        .await
        .unwrap();

    assert!(!lab.objects.lock().unwrap().contains_key(&key(bytes)));
}

/// A store-list failure is not a completed collection. Once the store recovers, S7 allows only
/// the grant TTL plus one interval, not a fresh grace period from the failed pass.
// contract: S7
#[tokio::test]
async fn a_failed_collection_does_not_restart_the_recovery_grace() {
    let bytes = b"round 24 failed collection";
    let (lab, deleted_at) = lab_with_deleted_chunk(bytes).await;
    let failed_at = deleted_at + GRACE_MS + INTERVAL_MS;
    lab.store_lists_fail.store(true, Ordering::SeqCst);
    assert!(lab.server.sweep(failed_at, &POLICY).await.is_err());

    lab.store_lists_fail.store(false, Ordering::SeqCst);
    let resumed = failed_at + INTERVAL_MS;
    lab.server.sweep(resumed, &POLICY).await.unwrap();
    lab.server
        .sweep(resumed + GRANT_TTL_MS + INTERVAL_MS, &POLICY)
        .await
        .unwrap();

    assert!(
        !lab.objects.lock().unwrap().contains_key(&key(bytes)),
        "the chunk remains past the post-resumption deadline"
    );
}

/// A forward wall-clock step followed by a correction must not leave the persisted unreferenced
/// time in the future and defeat S7 while collections continue at every interval.
// contract: S7
#[tokio::test]
async fn a_forward_clock_step_does_not_extend_the_erasure_deadline() {
    let bytes = b"round 24 clock correction";
    let (lab, deleted_at) = lab_with_deleted_chunk(bytes).await;
    lab.server
        .sweep(deleted_at + 10 * GRACE_MS, &POLICY)
        .await
        .unwrap();
    let server = lab.restarted_server();

    let mut collection = deleted_at + INTERVAL_MS;
    let deadline = deleted_at + GRACE_MS + INTERVAL_MS;
    while collection <= deadline {
        server.sweep(collection, &POLICY).await.unwrap();
        collection += INTERVAL_MS;
    }

    assert!(
        !lab.objects.lock().unwrap().contains_key(&key(bytes)),
        "the chunk remains after grace plus one interval of regular collections"
    );
}

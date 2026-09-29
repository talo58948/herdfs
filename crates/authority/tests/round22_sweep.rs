//! Red team, round 22: a late first collection after a space deletion.
#![allow(clippy::unwrap_used, reason = "test code")]

mod http;

use axum::http::StatusCode;
use http::{Lab, now_ms};
use herdfs_authority::Policy;
use herdfs_core::ChunkId;
use serde_json::json;
use std::time::Duration;

const GRACE_MS: i64 = 60 * 60 * 1_000;
const INTERVAL_MS: i64 = 10 * 60 * 1_000;
const POLICY: Policy = Policy {
    versions: 1,
    max_age_ms: 0,
    grace_ms: GRACE_MS,
};

fn key(bytes: &[u8]) -> String {
    format!("herdfs/chunks/{}", ChunkId::of(bytes))
}

async fn delete_chunk_space(lab: &Lab, bytes: &[u8]) -> i64 {
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
    now_ms()
}

/// A restart just before the next collection resets its schedule, but the persisted preceding
/// collection still lets the restarted collector meet the original deletion deadline.
// contract: S7
#[tokio::test]
async fn a_restart_between_deletion_and_collection_keeps_the_erasure_deadline() {
    const RESTART_GRACE_MS: i64 = 1_000;
    const RESTART_INTERVAL_MS: i64 = 200;
    const GRANT_TTL_MS: u64 = 20;
    let policy = Policy {
        versions: 1,
        max_age_ms: 0,
        grace_ms: RESTART_GRACE_MS,
    };
    let lab = Lab::start_with_ttl(Duration::from_millis(GRANT_TTL_MS)).await;
    let previous = now_ms();
    lab.server.sweep(previous, &policy).await.unwrap();
    let bytes = b"round 22 restart collection";
    let deleted_at = delete_chunk_space(&lab, bytes).await;

    tokio::time::sleep(Duration::from_millis(10)).await;
    let restarted_at = now_ms();
    assert!(restarted_at < deleted_at + RESTART_INTERVAL_MS);
    let server = lab.restarted_server();
    let deadline = deleted_at + RESTART_GRACE_MS + RESTART_INTERVAL_MS;
    let mut started = restarted_at + RESTART_INTERVAL_MS;
    while started <= deadline {
        server.sweep(started, &policy).await.unwrap();
        started += RESTART_INTERVAL_MS;
    }

    assert!(!lab.objects.lock().unwrap().contains_key(&key(bytes)));
}

/// After a collector stalls past the normal bound, the URL-safety clamp delays deletion until
/// no outstanding grant can survive, then the next collection erases the chunk.
// contract: S7
#[tokio::test]
async fn a_stalled_collector_erases_within_the_resumption_bound() {
    let lab = Lab::start().await;
    let bytes = b"round 22 late first collection";
    let deleted_at = delete_chunk_space(&lab, bytes).await;

    let resumed_at = deleted_at + GRACE_MS + INTERVAL_MS + 1;
    let resumed = lab.server.sweep(resumed_at, &POLICY).await.unwrap();
    assert_eq!(resumed.deleted, 0, "outstanding grants stay safe");
    let swept = lab.server.sweep(resumed_at + INTERVAL_MS, &POLICY).await.unwrap();
    assert_eq!(swept.deleted, 1);
    assert!(!lab.objects.lock().unwrap().contains_key(&key(bytes)));
}

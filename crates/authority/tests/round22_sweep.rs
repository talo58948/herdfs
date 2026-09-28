//! Red team, round 22: a late first collection after a space deletion.
#![allow(clippy::unwrap_used, reason = "test code")]

mod http;

use axum::http::StatusCode;
use http::{Lab, now_ms};
use herdfs_authority::Policy;
use herdfs_core::ChunkId;
use serde_json::json;

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
    assert_eq!(
        lab.rpc(&alice, &json!({"op": "create_space", "space": "team"}))
            .await
            .0,
        StatusCode::OK
    );
    let (_, file) = lab
        .rpc(
            &alice,
            &json!({"op": "create", "space": "team", "path": "f", "mode": 420}),
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

/// A collection within the interval records the deletion, and the first collection after the
/// grace then erases the chunk.
// contract: S7
#[tokio::test]
async fn regular_collections_erase_a_deleted_spaces_chunk_by_the_deadline() {
    let lab = Lab::start().await;
    let bytes = b"round 22 regular collection";
    let deleted_at = delete_chunk_space(&lab, bytes).await;

    let first = lab.server.sweep(deleted_at + INTERVAL_MS, &POLICY).await.unwrap();
    assert_eq!(first.deleted, 0);
    let deadline = deleted_at + GRACE_MS + INTERVAL_MS + 1;
    let second = lab.server.sweep(deadline, &POLICY).await.unwrap();
    assert_eq!(second.deleted, 1);
    assert!(!lab.objects.lock().unwrap().contains_key(&key(bytes)));
}

/// Repro: delete a space before this process has collected, then make its first collection just
/// after grace plus interval. S7's deadline has passed, but the sweep treats the late collection
/// as the start of a fresh grace window and leaves the sole chunk in the bucket.
// contract: S7
#[tokio::test]
#[ignore = "ROUND22: a late first collection restarts S7's grace after DeleteSpace"]
async fn a_late_first_collection_still_meets_the_erasure_deadline() {
    let lab = Lab::start().await;
    let bytes = b"round 22 late first collection";
    let deleted_at = delete_chunk_space(&lab, bytes).await;

    let deadline = deleted_at + GRACE_MS + INTERVAL_MS + 1;
    let swept = lab.server.sweep(deadline, &POLICY).await.unwrap();
    assert_eq!(swept.deleted, 1, "the S7 deadline has passed");
    assert!(!lab.objects.lock().unwrap().contains_key(&key(bytes)));
}

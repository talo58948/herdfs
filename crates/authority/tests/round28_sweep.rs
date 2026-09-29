//! Red team, round 28: an upload becomes retained and leaves retention between collections.
#![allow(clippy::unwrap_used, reason = "test code")]

mod http;

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use http::{Lab, now_ms};
use herdfs_authority::Policy;
use herdfs_core::ChunkId;
use serde_json::{Value, json};

const GRACE_MS: i64 = 12_000;
const POLICY: Policy = Policy {
    versions: 1,
    max_age_ms: 0,
    grace_ms: GRACE_MS,
};

fn key(bytes: &[u8]) -> String {
    format!("herdfs/chunks/{}", ChunkId::of(bytes))
}

async fn create_file(lab: &Lab, alice: &str, space: &str) -> Value {
    let root = lab.create_space(alice, space).await;
    let (status, file) = lab
        .rpc(
            alice,
            &json!({"op": "create", "space": space, "parent": root, "name": "f", "mode": 420}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{file}");
    file["entry"]["id"].clone()
}

async fn commit(lab: &Lab, alice: &str, space: &str, entry: &Value, request: &str, bytes: &[u8]) -> Value {
    let chunk = json!({"id": ChunkId::of(bytes).to_string(), "len": bytes.len()});
    let body = json!({"op": "commit", "space": space, "entry": entry,
        "request_id": request, "base": null, "guarded": false, "chunks": [chunk]});
    let (status, version) = lab.rpc(alice, &body).await;
    assert_eq!(status, StatusCode::OK, "{version}");
    version["version"]["id"].clone()
}

async fn grant(lab: &Lab, alice: &str, space: &str, entry: &Value, version: &Value) {
    let body = json!({"op": "grant", "space": space, "entry": entry,
        "version": version, "chunks": [0]});
    let (status, reply) = lab.rpc(alice, &body).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
}

async fn delete_space(lab: &Lab, alice: &str, space: &str) {
    let (status, reply) = lab
        .rpc(alice, &json!({"op": "delete_space", "space": space}))
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
}

/// A chunk uploaded but not yet committed at one collection is committed, granted and deleted
/// before the next. Its fresh grant must keep it just like the control chunk that was retained
/// at the preceding collection.
// contract: S4, S7
#[tokio::test]
async fn a_chunk_committed_between_collections_outlives_its_fresh_grant() {
    let lab = Lab::start_with_ttl(Duration::from_secs(5)).await;
    let alice = lab.user("alice");
    let between = b"round 28 retained between collections";
    let control = b"round 28 retained at collection";
    let between_entry = create_file(&lab, &alice, "between").await;
    let control_entry = create_file(&lab, &alice, "control").await;

    for (space, bytes) in [("between", between.as_slice()), ("control", control.as_slice())] {
        let (status, reply) = lab
            .upload(&alice, space, ChunkId::of(bytes), bytes.to_vec())
            .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
    }
    let control_version = commit(&lab, &alice, "control", &control_entry, "control", control).await;

    let first = now_ms();
    lab.server.sweep(first, &POLICY).await.unwrap();
    let between_version = commit(&lab, &alice, "between", &between_entry, "between", between).await;
    let granted = Instant::now();
    grant(&lab, &alice, "between", &between_entry, &between_version).await;
    grant(&lab, &alice, "control", &control_entry, &control_version).await;
    delete_space(&lab, &alice, "between").await;
    delete_space(&lab, &alice, "control").await;

    lab.server.sweep(first + GRACE_MS + 1, &POLICY).await.unwrap();

    assert!(granted.elapsed() < Duration::from_secs(5));
    let objects = lab.objects.lock().unwrap();
    assert!(
        objects.contains_key(&key(control)),
        "control grant lost its chunk"
    );
    assert!(
        objects.contains_key(&key(between)),
        "the chunk was deleted while its fresh grant remained valid"
    );
}

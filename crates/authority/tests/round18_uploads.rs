//! Red team, round 18 probe: odd request ids in the upload path, as raw URL text. Held; it stays
//! as a regression test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod http;

use axum::http::StatusCode;
use http::Lab;
use herdfs_core::ChunkId;
use serde_json::{Value, json};

/// Uploads `bytes` as `token` for the commit named by `request_id`, sent as written.
async fn upload(lab: &Lab, token: &str, request_id: &str, bytes: &[u8]) -> (StatusCode, Value) {
    let id = ChunkId::of(bytes);
    let url = format!("{}/v1/spaces/team/commits/{request_id}/chunks/{id}", lab.url);
    let reply = lab
        .http
        .put(url)
        .bearer_auth(token)
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    let status = reply.status();
    (status, reply.json().await.unwrap_or(Value::Null))
}

fn written(lab: &Lab, bytes: &[u8]) -> bool {
    let key = format!("herdfs/chunks/{}", ChunkId::of(bytes));
    lab.objects.lock().unwrap().contains_key(&key)
}

/// Encoded slashes, `..`, NUL, spaces, `?`, `#`, non-ASCII and an 8 KiB id each get the same
/// answer and a store write, and a commit may use every such chunk (S6). A segment that does not
/// decode to UTF-8 is refused before anything is recorded or written.
// contract: S6, C15
#[tokio::test]
async fn odd_request_ids_in_the_upload_path_are_answered_alike() {
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    let root = lab.create_space(&alice, "team").await;
    let long = "r".repeat(8 << 10);
    let ids = [
        "plain",
        "a%2Fb",
        "..%2F",
        "%00",
        "%20",
        "%3F%23",
        "%E2%80%AE%C3%BC",
        &long,
    ];
    let mut chunks = Vec::new();
    let mut answers = Vec::new();
    for (n, request_id) in ids.iter().enumerate() {
        let bytes = format!("chunk {n}").into_bytes();
        answers.push(upload(&lab, &alice, request_id, &bytes).await);
        assert!(written(&lab, &bytes), "{request_id}");
        chunks.push(json!({"id": ChunkId::of(&bytes).to_string(), "len": bytes.len()}));
    }
    assert!(answers.iter().all(|answer| *answer == answers[0]), "{answers:?}");
    assert_eq!(answers[0].0, StatusCode::OK);

    let refused = upload(&lab, &alice, "%FF", b"not utf-8").await;
    assert_eq!(refused.0, StatusCode::BAD_REQUEST, "{refused:?}");
    assert!(!written(&lab, b"not utf-8"));

    let create = json!({"op": "create", "space": "team", "parent": root, "name": "f", "mode": 420});
    let (_, file) = lab.rpc(&alice, &create).await;
    let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"],
        "request_id": "r1", "base": null, "guarded": false, "chunks": chunks});
    let (status, version) = lab.rpc(&alice, &commit).await;
    assert_eq!(status, StatusCode::OK, "{version}");
}

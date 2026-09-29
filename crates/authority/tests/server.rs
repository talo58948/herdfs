//! The HTTP routes: chunk uploads, presigned grants and error statuses.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod http;

use axum::http::StatusCode;
use http::{Lab, PUBLIC_S3, now_ms};
use herdfs_core::{ChunkId, chunk::MAX_CHUNK};
use serde_json::{Value, json};

#[tokio::test]
async fn only_a_caller_who_may_upload_causes_a_store_write() {
    // contract: S6, S1
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    lab.create_space(&alice, "team").await;
    let bytes = b"chunk bytes".to_vec();
    let id = ChunkId::of(&bytes);
    let refused = [
        (
            alice.as_str(),
            "team",
            b"other bytes".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (&alice, "team", vec![0; MAX_CHUNK + 1], StatusCode::BAD_REQUEST),
        ("bad", "team", bytes.clone(), StatusCode::UNAUTHORIZED),
        (&lab.user("bob"), "team", bytes.clone(), StatusCode::FORBIDDEN),
        (&alice, "nowhere", bytes.clone(), StatusCode::NOT_FOUND),
    ];
    for (token, space, body, status) in refused {
        assert_eq!(lab.upload(token, space, id, body).await.0, status);
    }
    assert!(lab.objects.lock().unwrap().is_empty());
    // Every accepted upload is written, even of bytes already stored.
    for _ in 0..2 {
        let reply = lab.upload(&alice, "team", id, bytes.clone()).await;
        assert_eq!(reply, (StatusCode::OK, json!({"type": "done"})));
    }
    assert_eq!(
        lab.objects
            .lock()
            .unwrap()
            .get(&format!("herdfs/chunks/{id}"))
            .map(|stored| stored.writes),
        Some(2)
    );
}

#[tokio::test]
async fn an_uploaded_chunk_is_committable_and_granted_on_the_public_endpoint() {
    // contract: S6, S2, S1
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    let root = lab.create_space(&alice, "team").await;
    let bytes = b"chunk bytes".to_vec();
    let id = ChunkId::of(&bytes);
    let key = format!("herdfs/chunks/{id}");
    assert_eq!(
        lab.upload(&alice, "team", id, bytes.clone()).await.0,
        StatusCode::OK
    );

    let (_, file) = lab
        .rpc(
            &alice,
            &json!({"op": "create", "space": "team", "parent": root, "name": "f", "mode": 420}),
        )
        .await;
    let chunks = json!([{"id": id.to_string(), "len": bytes.len()}]);
    let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"], "request_id": "r1",
        "base": null, "guarded": false, "chunks": chunks});
    let before_ms = now_ms();
    let (status, version) = lab.rpc(&alice, &commit).await;
    assert_eq!(status, StatusCode::OK, "{version}");
    // The server stamps versions with the wall clock.
    let created_ms = version["version"]["created_ms"].as_i64().unwrap();
    assert!(
        (before_ms..=now_ms()).contains(&created_ms),
        "created at {created_ms}"
    );

    let grant = json!({"op": "grant", "space": "team", "entry": file["entry"]["id"],
        "version": version["version"]["id"], "chunks": [0]});
    let (status, grants) = lab.rpc(&alice, &grant).await;
    assert_eq!(status, StatusCode::OK, "{grants}");
    let url = grants["urls"][0].as_str().unwrap();
    // contract: S2, S4 — presigned for the public endpoint, valid for the 60 s grant TTL.
    assert!(url.starts_with(&format!("{PUBLIC_S3}/{key}?")), "{url}");
    assert!(url.contains("X-Amz-Expires=60"), "{url}");
    assert_eq!(grants["ttl_ms"], json!(60_000));
}

#[tokio::test]
async fn errors_carry_their_code_and_status() {
    let lab = Lab::start().await;
    let (alice, bob) = (lab.user("alice"), lab.user("bob"));
    let root = lab.create_space(&alice, "team").await;
    let create = json!({"op": "create", "space": "team", "parent": root, "name": "f", "mode": 420});
    let stat = |space: &str, path: &str| json!({"op": "stat", "space": space, "path": path});
    let cases = [
        (&alice, create.clone(), StatusCode::OK, Value::Null),
        (&alice, create, StatusCode::CONFLICT, json!("exists")),
        (
            &alice,
            stat("team", "missing"),
            StatusCode::NOT_FOUND,
            json!("not_found"),
        ),
        (
            &alice,
            stat("nowhere", ""),
            StatusCode::NOT_FOUND,
            json!("not_found"),
        ),
        (&bob, stat("team", "f"), StatusCode::FORBIDDEN, json!("denied")),
        (
            &alice,
            json!({"op": "remove", "space": "team", "parent": root, "name": ""}),
            StatusCode::BAD_REQUEST,
            json!("invalid"),
        ),
        (
            &alice,
            json!({"op": "stat", "space": "team", "path": "../x"}),
            StatusCode::BAD_REQUEST,
            json!("invalid"),
        ),
    ];
    for (who, request, status, code) in cases {
        let (got, reply) = lab.rpc(who, &request).await;
        assert_eq!(got, status, "{request} answered {reply}");
        if !code.is_null() {
            assert_eq!(reply["code"], code, "{request}");
        }
    }
    let health = lab
        .http
        .get(format!("{}/v1/health", lab.url))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
}

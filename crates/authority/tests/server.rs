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
async fn uploads_are_verified_always_written_then_committable() {
    // contract: S6, S1
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    lab.rpc(&alice, &json!({"op": "create_space", "space": "team"}))
        .await;
    let bytes = b"chunk bytes".to_vec();
    let id = ChunkId::of(&bytes);
    let key = format!("herdfs/chunks/{id}");

    let (status, error) = lab.upload(&alice, "team", id, b"other bytes".to_vec()).await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::BAD_REQUEST, &json!("invalid"))
    );
    assert_eq!(
        lab.upload(&alice, "team", id, vec![0; MAX_CHUNK + 1]).await.0,
        StatusCode::BAD_REQUEST
    );
    assert!(lab.objects.lock().unwrap().is_empty());
    assert_eq!(
        lab.upload("bad", "team", id, bytes.clone()).await.0,
        StatusCode::UNAUTHORIZED
    );

    assert_eq!(
        lab.upload(&alice, "team", id, bytes.clone()).await,
        (StatusCode::OK, json!({"type": "done"}))
    );
    assert_eq!(
        lab.upload(&alice, "team", id, bytes.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(lab.objects.lock().unwrap().get(&key), Some(&2));
    // Without write in the space, the bytes are still stored but not recorded as bob's.
    assert_eq!(
        lab.upload(&lab.user("bob"), "team", id, bytes.clone()).await.0,
        StatusCode::FORBIDDEN
    );

    let (_, file) = lab
        .rpc(
            &alice,
            &json!({"op": "create", "space": "team", "path": "f", "mode": 420}),
        )
        .await;
    let chunks = json!([{"id": id.to_string(), "len": bytes.len()}]);
    let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"], "request_id": "r1",
        "base": null, "guarded": false, "chunks": chunks});
    let (status, version) = lab.rpc(&alice, &commit).await;
    assert_eq!(status, StatusCode::OK, "{version}");

    let grant = json!({"op": "grant", "space": "team", "path": "f", "version": version["version"]["id"], "chunks": [0]});
    let before_ms = now_ms();
    let (status, grants) = lab.rpc(&alice, &grant).await;
    let after_ms = now_ms();
    assert_eq!(status, StatusCode::OK, "{grants}");
    let url = grants["urls"][0].as_str().unwrap();
    // contract: S2 — presigned for the public endpoint, valid five minutes.
    assert!(url.starts_with(&format!("{PUBLIC_S3}/{key}?")), "{url}");
    assert!(url.contains("X-Amz-Expires=300"), "{url}");
    let expires_ms = grants["expires_ms"].as_i64().unwrap();
    assert!((before_ms + 300_000..=after_ms + 300_000).contains(&expires_ms));
}

#[tokio::test]
async fn errors_carry_their_code_and_status() {
    let lab = Lab::start().await;
    let (alice, bob) = (lab.user("alice"), lab.user("bob"));
    lab.rpc(&alice, &json!({"op": "create_space", "space": "team"}))
        .await;
    let create = json!({"op": "create", "space": "team", "path": "f", "mode": 420});
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
            json!({"op": "remove", "space": "team", "path": ""}),
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

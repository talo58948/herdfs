//! Red team, round 22: client-visible failures from signed object-store requests.
#![allow(clippy::unwrap_used, reason = "test code")]

mod http;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use http::Lab;
use herdfs_authority::{Authority, server::Server};
use herdfs_core::ChunkId;
use reqwest::Url;
use rusty_s3::{Bucket, Credentials, UrlStyle};
use serde_json::json;

async fn serve(server: Arc<Server>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, server.router()).await });
    url
}

/// A successful upload returns only the protocol acknowledgement, not its internal signed PUT.
// contract: S1
#[tokio::test]
async fn a_successful_upload_reveals_no_object_store_capability() {
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    assert_eq!(
        lab.rpc(&alice, &json!({"op": "create_space", "space": "team"}))
            .await
            .0,
        StatusCode::OK
    );
    let bytes = b"successful store request";
    let (status, answer) = lab
        .upload(&alice, "team", ChunkId::of(bytes), bytes.to_vec())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer, json!({"type": "done"}));
}

/// Repro: point the authority at a closed store port and upload a valid chunk. The returned 503
/// must not disclose the signed PUT capability that only the authority should hold.
// contract: S1
#[tokio::test]
#[ignore = "ROUND22: a failed store PUT exposes its signed URL to the client"]
async fn a_store_failure_reveals_no_object_store_capability() {
    let identity = Lab::start().await;
    let dir = tempfile::tempdir().unwrap();
    let endpoint = Url::parse("http://127.0.0.1:9").unwrap();
    let bucket = Bucket::new(endpoint, UrlStyle::Path, "herdfs", "us-east-1").unwrap();
    let server = Arc::new(Server::new(
        Authority::open(&dir.path().join("authority.db")).unwrap(),
        bucket.clone(),
        bucket,
        Credentials::new("round22-access", "round22-secret"),
        identity.issuer.clone(),
        "herdfs".into(),
        Duration::from_secs(60),
    ));
    let url = serve(server).await;
    let token = identity.user("alice");
    let created = identity
        .http
        .post(format!("{url}/v1/rpc"))
        .bearer_auth(&token)
        .json(&json!({"op": "create_space", "space": "team"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);

    let bytes = b"failed store request";
    let id = ChunkId::of(bytes);
    let failed = identity
        .http
        .put(format!("{url}/v1/spaces/team/commits/r1/chunks/{id}"))
        .bearer_auth(token)
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), StatusCode::SERVICE_UNAVAILABLE);
    let answer = failed.text().await.unwrap();
    for secret in ["X-Amz-", "round22-access", "round22-secret", "Signature="] {
        assert!(
            !answer.contains(secret),
            "client-visible error contains {secret:?}: {answer}"
        );
    }
}

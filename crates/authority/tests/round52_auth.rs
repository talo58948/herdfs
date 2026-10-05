//! Round 52: concurrent rotation that replaces a key without changing its id.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

mod http;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::{Json, Router, http::StatusCode, routing::get};
use http::{KEY, KEY_X, Lab, ROGUE_KEY, now};
use herdfs_authority::{Authority, server::Server};
use reqwest::Url;
use rusty_s3::{Bucket, Credentials, UrlStyle};
use serde_json::json;
use tokio::sync::{Notify, Semaphore};

const ROTATED_X: &str = "-jl_MBKrPoUjksv6ut9S1AkaQKUq-12zE9Lh4rS2aqs";

async fn whoami(authority: &str, token: &str) -> StatusCode {
    reqwest::Client::new()
        .post(format!("{authority}/v1/rpc"))
        .bearer_auth(token)
        .json(&json!({"op": "whoami"}))
        .send()
        .await
        .unwrap()
        .status()
}

async fn same_kid_trial() -> StatusCode {
    let fetches = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let jwks_uri = format!("{issuer}/jwks");
    let discovery = get(move || {
        let jwks_uri = jwks_uri.clone();
        async move { Json(json!({"jwks_uri": jwks_uri})) }
    });
    let jwks = get({
        let fetches = Arc::clone(&fetches);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        move || {
            let fetch = fetches.fetch_add(1, Ordering::SeqCst);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            async move {
                if fetch > 0 {
                    started.notify_one();
                    let _permit = release.acquire().await.unwrap();
                }
                let x = if fetch == 0 { KEY_X } else { ROTATED_X };
                Json(json!({"keys": [{
                    "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "kid": "stable", "x": x
                }]}))
            }
        }
    });
    let router = Router::new()
        .route("/.well-known/openid-configuration", discovery)
        .route("/jwks", jwks);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let bucket = Bucket::new(
        Url::parse("http://127.0.0.1:9").unwrap(),
        UrlStyle::Path,
        "herdfs",
        "us-east-1",
    )
    .unwrap();
    let server = Arc::new(Server::new(
        Authority::open(&dir.path().join("authority.db")).unwrap(),
        bucket.clone(),
        bucket,
        Credentials::new("key", "secret"),
        issuer.clone(),
        "herdfs".into(),
        Duration::from_secs(60),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, server.router()).await.unwrap() });
    let claims = json!({"iss": issuer, "aud": "herdfs", "sub": "alice", "exp": now() + 120});
    let old = Lab::token(&claims, KEY, "stable");
    let rotated = Lab::token(&claims, ROGUE_KEY, "stable");
    assert_eq!(whoami(&authority, &old).await, StatusCode::OK);

    tokio::time::sleep(Duration::from_secs(31)).await;
    let notified = started.notified();
    let refreshing = {
        let authority = authority.clone();
        let rotated = rotated.clone();
        tokio::spawn(async move { whoami(&authority, &rotated).await })
    };
    tokio::time::timeout(Duration::from_secs(2), notified)
        .await
        .unwrap();
    assert_eq!(
        whoami(&authority, &old).await,
        StatusCode::OK,
        "cached-key control"
    );
    let concurrent = {
        let authority = authority.clone();
        let rotated = rotated.clone();
        tokio::spawn(async move { whoami(&authority, &rotated).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    release.add_permits(1);
    assert_eq!(refreshing.await.unwrap(), StatusCode::OK, "refresh control");
    assert_eq!(
        whoami(&authority, &rotated).await,
        StatusCode::OK,
        "published-key control"
    );
    tokio::time::timeout(Duration::from_secs(2), concurrent)
        .await
        .expect("the concurrent request remained stuck after the refresh")
        .unwrap()
}

/// Requests signed by a replacement for a cached key must join the refresh publishing it, even
/// when the issuer kept the key id; cached old-key requests remain the nonblocking control.
// contract: S5
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "ROUND52: same-kid rotated requests do not wait for the in-flight refresh"]
async fn concurrent_requests_wait_when_rotated_key_reuses_its_id() {
    let (first, second, third) = tokio::join!(same_kid_trial(), same_kid_trial(), same_kid_trial());
    assert_eq!([first, second, third], [StatusCode::OK; 3]);
}

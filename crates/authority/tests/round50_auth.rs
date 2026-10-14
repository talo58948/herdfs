//! Round 50: concurrent requests for a key being added by an in-flight JWKS refresh.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

mod http;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::{Json, Router, http::StatusCode, routing::get};
use herdfs_authority::{Authority, server::Server};
use http::{KEY, KEY_X, Lab, ROGUE_KEY, now};
use reqwest::Url;
use rusty_s3::{Bucket, Credentials, UrlStyle};
use serde_json::json;
use tokio::sync::{Notify, Semaphore};

const ROGUE_X: &str = "-jl_MBKrPoUjksv6ut9S1AkaQKUq-12zE9Lh4rS2aqs";

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

async fn rotation_trial() -> StatusCode {
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
                let (kid, x) = if fetch == 0 {
                    ("k1", KEY_X)
                } else {
                    ("k2", ROGUE_X)
                };
                Json(json!({"keys": [{
                    "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "kid": kid, "x": x
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
    let old = Lab::token(&claims, KEY, "k1");
    let rotated = Lab::token(&claims, ROGUE_KEY, "k2");
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
        tokio::time::timeout(Duration::from_secs(1), whoami(&authority, &old))
            .await
            .expect("a cached known key waited for the refresh"),
        StatusCode::OK
    );
    let concurrent = {
        let authority = authority.clone();
        let rotated = rotated.clone();
        tokio::spawn(async move { whoami(&authority, &rotated).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    release.add_permits(1);
    assert_eq!(refreshing.await.unwrap(), StatusCode::OK);
    assert_eq!(whoami(&authority, &rotated).await, StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(2), concurrent)
        .await
        .expect("a rotated-key request remained stuck after the refresh")
        .unwrap()
}

/// Requests whose `kid` is absent from the usable cache must join the single in-flight refresh;
/// a request for an already cached key remains the nonblocking control.
// contract: S5
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_rotated_key_requests_wait_for_the_same_refresh() {
    let (first, second, third) = tokio::join!(rotation_trial(), rotation_trial(), rotation_trial());
    assert_eq!([first, second, third], [StatusCode::OK; 3]);
}

//! Round 48: concurrent issuer recovery and the expiry-leeway boundary.
#![allow(clippy::unwrap_used, reason = "test code")]

mod http;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::{Json, Router, http::StatusCode, routing::get};
use http::{KEY, KEY_X, Lab, now};
use herdfs_authority::{Authority, server::Server};
use reqwest::Url;
use rusty_s3::{Bucket, Credentials, UrlStyle};
use serde_json::json;
use tokio::sync::{Notify, Semaphore};

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

async fn flapping_server() -> (
    String,
    String,
    Arc<AtomicUsize>,
    Arc<Notify>,
    Arc<Semaphore>,
    tempfile::TempDir,
) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let discovery_url = format!("{issuer}/jwks");
    let discovery = {
        let attempts = Arc::clone(&attempts);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        move || {
            let attempts = Arc::clone(&attempts);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let discovery_url = discovery_url.clone();
            async move {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    started.notify_one();
                    let _permit = release.acquire().await.unwrap();
                    return Err(StatusCode::SERVICE_UNAVAILABLE);
                }
                Ok(Json(json!({"jwks_uri": discovery_url})))
            }
        }
    };
    let keys = get(|| async {
        Json(json!({"keys": [{
            "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "kid": "k1", "x": KEY_X
        }]}))
    });
    let issuer_router = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", keys);
    tokio::spawn(async move { axum::serve(listener, issuer_router).await.unwrap() });

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
    let authority = serve(server.router()).await;
    (issuer, authority, attempts, started, release, dir)
}

fn token(issuer: &str) -> String {
    let claims = json!({"iss": issuer, "aud": "herdfs", "sub": "alice", "exp": now() + 60});
    Lab::token(&claims, KEY, "k1")
}

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

/// One failed refresh must not make concurrent cold requests wait 30 seconds: one waiter retries,
/// all the rest share its recovered keys, and later requests use the cache.
// contract: S5
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_recover_single_flight_after_the_issuer_flaps() {
    for trial in 0..3 {
        let (issuer, authority, attempts, started, release, _dir) = flapping_server().await;
        let token = token(&issuer);
        let first = {
            let authority = authority.clone();
            let token = token.clone();
            tokio::spawn(async move { whoami(&authority, &token).await })
        };
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();
        let waiters: Vec<_> = (0..8)
            .map(|_| {
                let authority = authority.clone();
                let token = token.clone();
                tokio::spawn(async move { whoami(&authority, &token).await })
            })
            .collect();
        release.add_permits(1);

        assert_eq!(
            first.await.unwrap(),
            StatusCode::SERVICE_UNAVAILABLE,
            "trial {trial}"
        );
        for waiter in waiters {
            assert_eq!(waiter.await.unwrap(), StatusCode::OK, "trial {trial}");
        }
        assert_eq!(whoami(&authority, &token).await, StatusCode::OK);
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "trial {trial}");
    }
}

/// The allowed five seconds are a real boundary: a token three seconds past `exp` is the passing
/// control, while one six seconds past it is never accepted.
// contract: S5
#[tokio::test]
async fn expiry_leeway_never_exceeds_five_seconds() {
    let lab = Lab::start().await;
    for trial in 0..3 {
        let expired = |seconds| {
            let claims = json!({
                "iss": lab.issuer, "aud": "herdfs", "sub": "alice", "exp": now() - seconds
            });
            Lab::token(&claims, KEY, "k1")
        };
        assert_eq!(
            lab.rpc(&expired(3), &json!({"op": "whoami"})).await.0,
            StatusCode::OK
        );
        assert_eq!(
            lab.rpc(&expired(6), &json!({"op": "whoami"})).await.0,
            StatusCode::UNAUTHORIZED,
            "trial {trial}"
        );
    }
}

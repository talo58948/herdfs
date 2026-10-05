//! Round 54: key withdrawal while old- and new-key requests race one refresh.
#![allow(clippy::unwrap_used, reason = "test code")]

mod http;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use axum::{Json, Router, http::StatusCode, routing::get};
use http::{KEY, KEY_X, Lab, ROGUE_KEY, now};
use herdfs_authority::{Authority, server::Server};
use reqwest::Url;
use rusty_s3::{Bucket, Credentials, UrlStyle};
use serde_json::json;

const NEW_X: &str = "-jl_MBKrPoUjksv6ut9S1AkaQKUq-12zE9Lh4rS2aqs";
const NONE_WITH_KID: &str = "eyJhbGciOiJub25lIiwia2lkIjoiazEiLCJ0eXAiOiJKV1QifQ";

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

async fn withdrawal_trial(trial: usize) {
    let withdrawn = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let jwks_uri = format!("{issuer}/jwks");
    let discovery = get(move || {
        let jwks_uri = jwks_uri.clone();
        async move { Json(json!({"jwks_uri": jwks_uri})) }
    });
    let jwks = get({
        let withdrawn = Arc::clone(&withdrawn);
        move || {
            let new = withdrawn.load(Ordering::SeqCst);
            async move {
                let (kid, x) = if new { ("new", NEW_X) } else { ("old", KEY_X) };
                Json(json!({"keys": [{
                    "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "kid": kid, "x": x
                }]}))
            }
        }
    });
    let issuer_router = Router::new()
        .route("/.well-known/openid-configuration", discovery)
        .route("/jwks", jwks);
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, server.router()).await.unwrap() });
    let claims = json!({"iss": issuer, "aud": "herdfs", "sub": "alice", "exp": now() + 120});
    let old = Lab::token(&claims, KEY, "old");
    let new = Lab::token(&claims, ROGUE_KEY, "new");

    assert_eq!(
        whoami(&authority, &old).await,
        StatusCode::OK,
        "warm control {trial}"
    );
    tokio::time::sleep(Duration::from_secs(31)).await;
    withdrawn.store(true, Ordering::SeqCst);
    let (old_race, new_race) = tokio::join!(whoami(&authority, &old), whoami(&authority, &new));
    assert!(
        matches!(old_race, StatusCode::OK | StatusCode::UNAUTHORIZED),
        "old request failed unexpectedly in trial {trial}: {old_race}"
    );
    assert_eq!(new_race, StatusCode::OK, "new-key control {trial}");
    assert_eq!(
        whoami(&authority, &old).await,
        StatusCode::UNAUTHORIZED,
        "withdrawn key remained accepted after publication in trial {trial}"
    );
}

/// Once the single-flight refresh publishes a withdrawn-key set, later requests cannot keep
/// authenticating with the removed key; a request signed by its replacement is the control.
// contract: S5
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_published_refresh_withdraws_the_old_key_during_concurrent_requests() {
    tokio::join!(withdrawal_trial(0), withdrawal_trial(1), withdrawal_trial(2));
}

/// Supplying a known key id cannot make an unsigned `alg=none` token reach cached-key validation;
/// the normally signed token with the same claims is the control.
// contract: S5
#[tokio::test]
async fn alg_none_is_rejected_even_when_it_names_a_cached_key() {
    let lab = Lab::start().await;
    let signed = lab.user("alice");
    let payload = signed.split('.').nth(1).unwrap();
    let unsigned = format!("{NONE_WITH_KID}.{payload}.");
    for trial in 0..3 {
        assert_eq!(
            whoami(&lab.url, &unsigned).await,
            StatusCode::UNAUTHORIZED,
            "trial {trial}"
        );
        assert_eq!(
            whoami(&lab.url, &signed).await,
            StatusCode::OK,
            "signed control {trial}"
        );
    }
}

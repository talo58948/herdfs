//! Red team, round 1: confirmed breaks in the authority alone, each reduced to one failing test.
//! They are ignored so main stays green; each runs once its bug is fixed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod http;
mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{Json, Router, http::StatusCode, routing::get};
use http::{KEY, KEY_X, Lab, PUBLIC_S3, ROGUE_KEY, now};
use herdfs_authority::{Authority, server::Server};
use herdfs_core::proto::{ErrorCode, Request};
use reqwest::Url;
use rusty_s3::{Bucket, Credentials, UrlStyle};
use serde_json::{Value, json};
use support::{Fixture, SPACE, alice, path};

/// The public half of `ROGUE_KEY`, as a JWK's base64url `x`.
const ROGUE_X: &str = "-jl_MBKrPoUjksv6ut9S1AkaQKUq-12zE9Lh4rS2aqs";

/// Repro: sign a token that expired 30 s ago. jsonwebtoken validates `exp` with a default leeway
/// of 60 s, which `server/auth.rs` keeps, so the token is accepted.
// contract: S5
#[tokio::test]
#[ignore = "REDTEAM: a token is accepted for 60 s after its exp (jsonwebtoken's default leeway)"]
async fn a_token_past_its_expiry_is_refused() {
    let lab = Lab::start().await;
    let mut claims = lab.claims("alice");
    claims["exp"] = json!(now() - 30);
    let (status, _) = lab
        .rpc(&Lab::token(&claims, KEY, "k1"), &json!({"op": "whoami"}))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Repro: the issuer publishes keys k1 and k2, a k1 token is used, then the issuer withdraws k1,
/// as it does with a leaked key. The authority fetches the keys again only for a token naming an
/// unknown key id, so while tokens name k1 or k2 it trusts k1 for good; here past its own 30 s
/// refetch interval.
// contract: S5
#[tokio::test]
#[ignore = "REDTEAM: a key the issuer withdrew stays trusted until a token names an unknown key id"]
async fn a_key_the_issuer_withdrew_is_refused() {
    let jwk =
        |kid: &str, x: &str| json!({"kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "kid": kid, "x": x});
    let published = Arc::new(Mutex::new(vec![jwk("k1", KEY_X), jwk("k2", ROGUE_X)]));
    let (issuer, authority, _dir) = serve_with_keys(Arc::clone(&published)).await;
    let claims = json!({"iss": issuer, "aud": "herdfs", "sub": "alice", "exp": now() + 600});
    let whoami = |kid: &'static str, key: &'static str| {
        let (token, url) = (Lab::token(&claims, key, kid), format!("{authority}/v1/rpc"));
        async move {
            let call = reqwest::Client::new().post(url).bearer_auth(token);
            call.json(&json!({"op": "whoami"})).send().await.unwrap().status()
        }
    };
    assert_eq!(whoami("k1", KEY).await, StatusCode::OK);

    published.lock().unwrap().retain(|key| key["kid"] != "k1");
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(whoami("k2", ROGUE_KEY).await, StatusCode::OK);
    assert_eq!(whoami("k1", KEY).await, StatusCode::UNAUTHORIZED);
}

/// An issuer publishing `keys`, and an authority trusting it: their base URLs.
async fn serve_with_keys(keys: Arc<Mutex<Vec<Value>>>) -> (String, String, tempfile::TempDir) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let discovery = json!({"issuer": issuer, "jwks_uri": format!("{issuer}/jwks")});
    let jwks = move || async move { Json(json!({"keys": *keys.lock().unwrap()})) };
    let routes = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(|| async { Json(discovery) }),
        )
        .route("/jwks", get(jwks));
    tokio::spawn(async move { axum::serve(listener, routes).await });

    let dir = tempfile::tempdir().unwrap();
    let bucket = Bucket::new(
        Url::parse(PUBLIC_S3).unwrap(),
        UrlStyle::Path,
        "herdfs",
        "us-east-1",
    )
    .unwrap();
    let server = Server::new(
        Authority::open(&dir.path().join("authority.db")).unwrap(),
        bucket.clone(),
        bucket,
        Credentials::new("key", "secret"),
        issuer.clone(),
        "herdfs".into(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, server.router()).await });
    (issuer, authority, dir)
}

/// Repro: a subtree whose deepest path is 4042 bytes, then its top directory renamed to a
/// 255-byte name. The rename's SQL rewrites every path below without checking the 4096-byte
/// limit, and each path read back must parse as a `SpacePath`, so listing the deepest directory
/// fails with `Unavailable` ("database failure") from then on, for everyone.
// contract: C7
#[test]
#[ignore = "REDTEAM: a rename may push paths below it past 4096 bytes; listing them then fails"]
fn a_rename_leaves_every_path_below_it_usable() {
    let mut fx = Fixture::new();
    let mut deepest = "a".to_owned();
    fx.mkdir(&alice(), &deepest).unwrap();
    for _ in 0..15 {
        deepest = format!("{deepest}/{}", "d".repeat(255));
        fx.mkdir(&alice(), &deepest).unwrap();
    }
    fx.create(&alice(), &format!("{deepest}/{}", "f".repeat(200)))
        .unwrap();

    let top = "b".repeat(255);
    let renamed = fx.rename(&alice(), "a", &top);
    let list = Request::List {
        space: SPACE.into(),
        path: path(&deepest.replacen('a', &top, 1)),
    };
    let listed = fx.call(&alice(), list).map(drop);
    assert!(
        renamed.is_err() || listed.is_ok(),
        "the rename succeeded, then listing below it failed with {listed:?}"
    );
    assert_ne!(listed, Err(ErrorCode::Unavailable));
}

/// Repro: commit a version of 25,000 one-MiB chunks, a 24 GiB file, all from its base. The RPC
/// route keeps axum's default 2 MiB JSON body limit, so the commit is refused as `Invalid`,
/// which the client takes as final: the closed write is set aside and never commits.
// contract: C4a
#[tokio::test]
#[ignore = "REDTEAM: a commit of more than about 23,000 chunks (a 23 GiB file) exceeds the 2 MiB RPC limit"]
async fn a_commit_of_a_large_file_is_accepted() {
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    lab.rpc(&alice, &json!({"op": "create_space", "space": "team"}))
        .await;
    let create = json!({"op": "create", "space": "team", "path": "f", "mode": 420});
    let (_, created) = lab.rpc(&alice, &create).await;
    let chunk = vec![7; 1 << 20];
    let id = herdfs_core::ChunkId::of(&chunk);
    assert_eq!(lab.upload(&alice, "team", id, chunk).await.0, StatusCode::OK);
    let commit = |base: Value, chunks: usize| {
        let chunks = vec![json!({"id": id.to_string(), "len": 1 << 20}); chunks];
        let (entry, request_id) = (created["entry"]["id"].clone(), format!("r{}", chunks.len()));
        json!({"op": "commit", "space": "team", "entry": entry, "request_id": request_id,
               "base": base, "guarded": false, "chunks": chunks})
    };
    let (_, first) = lab.rpc(&alice, &commit(Value::Null, 1)).await;
    let large = commit(first["version"]["id"].clone(), 25_000);
    let url = format!("{}/v1/rpc", lab.url);
    let reply = lab
        .http
        .post(url)
        .bearer_auth(&alice)
        .json(&large)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK, "{}", reply.text().await.unwrap());
}

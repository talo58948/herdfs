//! Round 86: local-issuer verification must not trust proxy-substituted discovery keys.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{Json, Router, extract::State};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const PKCS8_PREFIX: &[u8] = &[
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

#[derive(Deserialize)]
struct Stored {
    issuer: String,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    aud: &'static str,
    sub: &'static str,
    exp: u64,
    jti: &'a str,
    groups: [&'static str; 1],
}

struct Authority(Child);

impl Drop for Authority {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Proxy environment must not let an attacker replace the built-in issuer's verification key;
/// a normally issued token without the proxy is the passing control.
// contract: S5
#[tokio::test]
#[ignore = "ROUND86: local issuer accepts a proxy-substituted signing key"]
async fn local_issuer_does_not_fetch_its_own_key_through_a_proxy() {
    let attacker = SigningKey::from_bytes(&rand::random());
    let proxy = proxy(attacker.verifying_key().as_bytes()).await;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let (authority, base) = start(&db, Some(&proxy)).await;
    let stored: Stored =
        serde_json::from_slice(&fs::read(dir.path().join("authority.db.local-issuer.json")).unwrap())
            .unwrap();
    let (id, _) = issue(&db);
    let forged = sign(&stored.issuer, &id, &attacker);

    let mut accepted = 0;
    for trial in 0..3 {
        let (status, reply) = rpc(&base, &forged).await;
        let forged_groups = reply["groups"] == json!(["admins"]);
        accepted += usize::from(status == StatusCode::OK && forged_groups);
        eprintln!("trial {trial}: forged token status = {status}, reply = {reply}");
    }
    drop(authority);

    let control_dir = tempfile::tempdir().unwrap();
    let control_db = control_dir.path().join("authority.db");
    let (_control, control_base) = start(&control_db, None).await;
    let (_, valid) = issue(&control_db);
    for trial in 0..3 {
        assert_eq!(
            rpc(&control_base, &valid).await.0,
            StatusCode::OK,
            "control {trial}"
        );
    }
    assert_eq!(accepted, 0, "{accepted}/3 forged tokens were accepted");
}

async fn proxy(public: &[u8; 32]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let jwks_uri = format!("{base}/keys");
    let key = URL_SAFE_NO_PAD.encode(public);
    let app = Router::new()
        .fallback(
            |State((jwks_uri, key)): State<(String, String)>, uri: axum::http::Uri| async move {
                let body = if uri.path().ends_with("openid-configuration") {
                    json!({"jwks_uri": jwks_uri})
                } else {
                    json!({"keys": [{
                        "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA",
                        "use": "sig", "kid": "local", "x": key
                    }]})
                };
                Json(body)
            },
        )
        .with_state((jwks_uri, key));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

async fn start(db: &Path, proxy: Option<&str>) -> (Authority, String) {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen = format!("127.0.0.1:{port}");
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"));
    command
        .args(["--listen", &listen, "--db"])
        .arg(db)
        .args([
            "--s3-endpoint",
            "http://127.0.0.1:9",
            "--s3-public-endpoint",
            "http://s3.lab:9",
            "--s3-bucket",
            "herdfs",
            "--s3-region",
            "us-east-1",
            "--local-issuer",
        ])
        .envs([("AWS_ACCESS_KEY_ID", "key"), ("AWS_SECRET_ACCESS_KEY", "secret")])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(proxy) = proxy {
        command
            .env("HTTP_PROXY", proxy)
            .env("http_proxy", proxy)
            .env("ALL_PROXY", proxy)
            .env("all_proxy", proxy)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy");
    }
    let authority = Authority(command.spawn().unwrap());
    let base = format!("http://{listen}");
    let started = Instant::now();
    loop {
        if reqwest::get(format!("{base}/v1/health"))
            .await
            .is_ok_and(|reply| reply.status().is_success())
        {
            return (authority, base);
        }
        assert!(started.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn issue(db: &Path) -> (String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["issue-token", "--db"])
        .arg(db)
        .args(["--user", "alice", "--days", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let id = String::from_utf8(output.stderr)
        .unwrap()
        .trim()
        .strip_prefix("token id: ")
        .unwrap()
        .to_owned();
    (id, String::from_utf8(output.stdout).unwrap().trim().to_owned())
}

fn sign(issuer: &str, id: &str, key: &SigningKey) -> String {
    let claims = Claims {
        iss: issuer,
        aud: "herdfs",
        sub: "alice",
        exp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 86_400,
        jti: id,
        groups: ["admins"],
    };
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some("local".into());
    let mut der = Vec::from(PKCS8_PREFIX);
    der.extend_from_slice(key.as_bytes());
    jsonwebtoken::encode(&header, &claims, &EncodingKey::from_ed_der(&der)).unwrap()
}

async fn rpc(base: &str, token: &str) -> (StatusCode, Value) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/rpc"))
        .bearer_auth(token)
        .json(&json!({"op": "whoami"}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let reply = response.json().await.unwrap();
    (status, reply)
}

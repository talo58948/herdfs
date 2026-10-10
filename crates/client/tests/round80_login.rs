//! Round 80: first login before the default token directory exists.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::Path;
use std::process::Stdio;

use axum::{
    Json, Router,
    routing::{get, post},
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;

async fn issuer() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let discovery_base = base.clone();
    let discovery = get(move || {
        let base = discovery_base.clone();
        async move {
            Json(json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token")
            }))
        }
    });
    let token = post(|| async {
        Json(json!({
            "access_token": "access",
            "refresh_token": "refresh",
            "expires_in": 3600
        }))
    });
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/.well-known/openid-configuration", discovery)
                .route("/token", token),
        )
        .await
        .unwrap();
    });
    base
}

async fn login(issuer: &str, home: &Path) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_herdfs"))
        .args([
            "login",
            "--issuer",
            issuer,
            "--client-id",
            "client",
            "--redirect-port",
            "0",
        ])
        .env("HOME", home)
        .env("PATH", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut authorize = String::new();
    stdout.read_line(&mut authorize).await.unwrap();
    let authorize = reqwest::Url::parse(authorize.trim()).unwrap();
    let parameter = |name| {
        authorize
            .query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
            .unwrap()
    };
    let redirect = reqwest::Url::parse(&parameter("redirect_uri")).unwrap();
    let address = format!("{}:{}", redirect.host_str().unwrap(), redirect.port().unwrap());
    let target = format!("{}?code=code&state={}", redirect.path(), parameter("state"));
    let mut callback = TcpStream::connect(address).await.unwrap();
    callback
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut reply = String::new();
    callback.read_to_string(&mut reply).await.unwrap();
    let output = child.wait_with_output().await.unwrap();
    (
        output.status.success(),
        format!(
            "callback={reply:?}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

/// A fresh default path must sign in just like the same path with its parent pre-created.
// contract: U1
#[tokio::test]
#[ignore = "ROUND80: first login fails before creating the default token directory"]
async fn first_login_creates_the_default_token_directory() {
    let issuer = issuer().await;
    let mut failed = 0;
    let mut failures = Vec::new();
    for trial in 0..3 {
        let fresh = tempfile::tempdir().unwrap();
        let (succeeded, detail) = login(&issuer, fresh.path()).await;
        let saved = fresh.path().join(".config/herdfs/token").exists();
        if !succeeded || !saved {
            failed += 1;
            failures.push(format!("fresh login {trial}: {detail}"));
        }

        let control = tempfile::tempdir().unwrap();
        fs::create_dir_all(control.path().join(".config/herdfs")).unwrap();
        let (succeeded, detail) = login(&issuer, control.path()).await;
        assert!(succeeded, "control login {trial}: {detail}");
        assert!(control.path().join(".config/herdfs/token").exists());
    }
    assert_eq!(failed, 0, "{failed}/3 fresh logins failed: {failures:#?}");
}

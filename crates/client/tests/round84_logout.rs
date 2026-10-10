//! Round 84: logout racing an in-flight token refresh.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use axum::{Json, Router, routing::post};
use serde_json::json;
use tokio::process::Command;
use tokio::sync::Semaphore;

struct Services {
    authority: String,
    token_endpoint: String,
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

async fn services() -> Services {
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let token_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_endpoint = format!("http://{}/token", token_listener.local_addr().unwrap());
    let token = post({
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        move || {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            async move {
                started.add_permits(1);
                release.acquire().await.unwrap().forget();
                Json(json!({
                    "access_token": "fresh-access",
                    "refresh_token": "fresh-refresh",
                    "expires_in": 3600
                }))
            }
        }
    });
    tokio::spawn(async move {
        axum::serve(token_listener, Router::new().route("/token", token))
            .await
            .unwrap();
    });

    let authority_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = format!("http://{}", authority_listener.local_addr().unwrap());
    let rpc = post(|| async { Json(json!({"type": "me", "principal": "user:alice", "groups": []})) });
    tokio::spawn(async move {
        axum::serve(authority_listener, Router::new().route("/v1/rpc", rpc))
            .await
            .unwrap();
    });
    Services {
        authority,
        token_endpoint,
        started,
        release,
    }
}

fn expired(path: &Path, endpoint: &str) {
    std::fs::write(
        path,
        serde_json::to_vec(&json!({
            "generation": 0,
            "access_token": "expired-access",
            "refresh_token": "old-refresh",
            "expires_at": 0,
            "client_id": "client",
            "token_endpoint": endpoint
        }))
        .unwrap(),
    )
    .unwrap();
}

fn whoami(authority: &str, token: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdfs"));
    command
        .args(["whoami", "--authority", authority, "--token-file"])
        .arg(token)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

async fn logout(token: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_herdfs"))
        .args(["logout", "--token-file"])
        .arg(token)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A successful logout must leave the saved login deleted even when another process was already
/// refreshing it; deleting an idle token after that refresh is the control.
// contract: U1
#[tokio::test]
#[ignore = "ROUND84: an in-flight refresh recreates a successfully logged-out token"]
async fn logout_wins_over_an_in_flight_refresh() {
    let mut recreated = 0;
    for trial in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        let service = services().await;
        expired(&token, &service.token_endpoint);
        let mut refresh = whoami(&service.authority, &token).spawn().unwrap();
        service.started.acquire().await.unwrap().forget();

        logout(&token).await;
        assert!(!token.exists(), "logout did not initially delete trial {trial}");
        service.release.add_permits(1);
        assert!(refresh.wait().await.unwrap().success());
        recreated += usize::from(token.exists());

        logout(&token).await;
        assert!(!token.exists(), "uncontended logout control {trial}");
    }
    assert_eq!(
        recreated, 0,
        "refresh recreated {recreated}/3 logged-out token files"
    );
}

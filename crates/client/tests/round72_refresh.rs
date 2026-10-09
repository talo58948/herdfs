//! Round 72: concurrent processes refreshing one rotating login token.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    routing::post,
};
use serde_json::json;
use tokio::process::Command;
use tokio::sync::Barrier;

async fn services() -> (String, String, Arc<AtomicUsize>) {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(3));
    let token_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_endpoint = format!("http://{}/token", token_listener.local_addr().unwrap());
    let token = post({
        let refreshes = Arc::clone(&refreshes);
        let barrier = Arc::clone(&barrier);
        move || {
            let request = refreshes.fetch_add(1, Ordering::SeqCst);
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                if request == 0 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "access_token": "fresh-access",
                            "refresh_token": "refresh-1",
                            "expires_in": 3600
                        })),
                    );
                }
                (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"})))
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
    let rpc = post(|headers: HeaderMap| async move {
        let fresh =
            headers.get(AUTHORIZATION).and_then(|value| value.to_str().ok()) == Some("Bearer fresh-access");
        if fresh {
            (
                StatusCode::OK,
                Json(json!({"type": "me", "principal": "user:alice", "groups": []})),
            )
        } else {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"code": "unauthenticated", "message": "stale token"})),
            )
        }
    });
    tokio::spawn(async move {
        axum::serve(authority_listener, Router::new().route("/v1/rpc", rpc))
            .await
            .unwrap();
    });
    (token_endpoint, authority, refreshes)
}

fn expired(path: &Path, endpoint: &str) {
    std::fs::write(
        path,
        serde_json::to_vec(&json!({
            "access_token": "expired-access",
            "refresh_token": "refresh-0",
            "expires_at": 0,
            "client_id": "client",
            "token_endpoint": endpoint
        }))
        .unwrap(),
    )
    .unwrap();
}

fn command(authority: &str, token: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdfs"));
    command
        .args(["whoami", "--authority", authority, "--token-file"])
        .arg(token)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

async fn race(authority: &str, token: &Path) -> Vec<Output> {
    let children: Vec<_> = (0..3)
        .map(|_| command(authority, token).spawn().unwrap())
        .collect();
    let mut outputs = Vec::new();
    for child in children {
        outputs.push(child.wait_with_output().await.unwrap());
    }
    outputs
}

/// Processes sharing one login must serialize a rotating refresh token, so one successful
/// refresh supplies every concurrent caller instead of exposing the issuer's single-use race.
// contract: U1
#[tokio::test]
#[ignore = "ROUND72: concurrent processes submit the same rotating refresh token"]
async fn concurrent_commands_share_one_rotating_refresh() {
    let mut failed_waves = 0;
    for trial in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        let (endpoint, authority, refreshes) = services().await;
        expired(&token, &endpoint);

        let outputs = race(&authority, &token).await;
        let succeeded = outputs.iter().filter(|output| output.status.success()).count();
        failed_waves += usize::from(succeeded != 3);
        assert_eq!(
            refreshes.load(Ordering::SeqCst),
            3,
            "refresh attempts in trial {trial}"
        );

        let control = command(&authority, &token).output().await.unwrap();
        assert!(
            control.status.success(),
            "uncontended control failed in trial {trial}"
        );
        assert_eq!(
            refreshes.load(Ordering::SeqCst),
            3,
            "control refreshed in trial {trial}"
        );
        eprintln!("trial {trial}: {succeeded}/3 concurrent commands succeeded");
    }
    assert_eq!(failed_waves, 0, "{failed_waves}/3 refresh waves lost callers");
}

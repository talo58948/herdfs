//! Round 99: a stale request crossing logout and a new browser login.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;

struct Services {
    authority: String,
    issuer: String,
    old_started: Arc<Semaphore>,
    release_old: Arc<Semaphore>,
    refreshes: Arc<AtomicUsize>,
}

async fn services() -> Services {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let issuer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", issuer_listener.local_addr().unwrap());
    let discovery_base = issuer.clone();
    let discovery = get(move || {
        let base = discovery_base.clone();
        async move {
            Json(json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token")
            }))
        }
    });
    let token = post({
        let refreshes = Arc::clone(&refreshes);
        move |body: Bytes| {
            let refreshes = Arc::clone(&refreshes);
            async move {
                if body
                    .windows(b"grant_type=refresh_token".len())
                    .any(|part| part == b"grant_type=refresh_token")
                {
                    refreshes.fetch_add(1, Ordering::SeqCst);
                    return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"})))
                        .into_response();
                }
                Json(json!({
                    "access_token": "new-access",
                    "refresh_token": "new-refresh",
                    "expires_in": 3600
                }))
                .into_response()
            }
        }
    });
    tokio::spawn(async move {
        axum::serve(
            issuer_listener,
            Router::new()
                .route("/.well-known/openid-configuration", discovery)
                .route("/token", token),
        )
        .await
        .unwrap();
    });

    let old_started = Arc::new(Semaphore::new(0));
    let release_old = Arc::new(Semaphore::new(0));
    let authority_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = format!("http://{}", authority_listener.local_addr().unwrap());
    let rpc = post({
        let old_started = Arc::clone(&old_started);
        let release_old = Arc::clone(&release_old);
        move |headers: HeaderMap| {
            let old_started = Arc::clone(&old_started);
            let release_old = Arc::clone(&release_old);
            async move {
                let token = headers
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.strip_prefix("Bearer "));
                if token == Some("old-access") {
                    old_started.add_permits(1);
                    release_old.acquire().await.unwrap().forget();
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({
                            "code": "unauthenticated", "message": "old token"
                        })),
                    )
                        .into_response();
                }
                if token == Some("new-access") {
                    return Json(json!({
                        "type": "me", "principal": "user:new", "groups": []
                    }))
                    .into_response();
                }
                StatusCode::UNAUTHORIZED.into_response()
            }
        }
    });
    tokio::spawn(async move {
        axum::serve(authority_listener, Router::new().route("/v1/rpc", rpc))
            .await
            .unwrap();
    });
    Services {
        authority,
        issuer,
        old_started,
        release_old,
        refreshes,
    }
}

fn old_token(path: &Path) {
    let expires_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 3600;
    std::fs::write(
        path,
        serde_json::to_vec(&json!({
            "generation": 7,
            "access_token": "old-access",
            "refresh_token": "old-refresh",
            "expires_at": expires_at,
            "client_id": "old-client",
            "token_endpoint": "http://127.0.0.1:9/token"
        }))
        .unwrap(),
    )
    .unwrap();
}

fn whoami(authority: &str, token: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_herdfs"))
        .args(["whoami", "--authority", authority, "--token-file"])
        .arg(token)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

async fn logout(token: &Path) {
    let status = Command::new(env!("CARGO_BIN_EXE_herdfs"))
        .args(["logout", "--token-file"])
        .arg(token)
        .status()
        .await
        .unwrap();
    assert!(status.success());
}

async fn login(issuer: &str, token: &Path) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_herdfs"))
        .args(["login", "--issuer", issuer, "--client-id", "new-client"])
        .args(["--redirect-port", "0", "--token-file"])
        .arg(token)
        .env("PATH", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
    assert!(reply.contains("200 OK"), "{reply}");
    assert!(child.wait().await.unwrap().success());
}

async fn trial(reset_generation: bool) -> (bool, usize, u64) {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    old_token(&token);
    let service = services().await;
    let mut request = whoami(&service.authority, &token);
    service.old_started.acquire().await.unwrap().forget();
    if reset_generation {
        logout(&token).await;
    }
    login(&service.issuer, &token).await;
    let saved: Value = serde_json::from_slice(&std::fs::read(&token).unwrap()).unwrap();
    service.release_old.add_permits(1);
    let succeeded = request.wait().await.unwrap().success();
    (
        succeeded,
        service.refreshes.load(Ordering::SeqCst),
        saved["generation"].as_u64().unwrap(),
    )
}

/// A completed login must supersede a request that failed under the previous login even after
/// logout removed its generation; replacing an existing login is the monotonic-generation control.
// contract: U1
#[tokio::test]
#[ignore = "ROUND99: logout resets generation so an older 401 refreshes the new login"]
async fn login_after_logout_wins_over_an_older_failed_request() {
    let mut failed = 0;
    for trial_number in 0..3 {
        let control = trial(false).await;
        assert_eq!(control, (true, 0, 8), "control {trial_number}");

        let attack = trial(true).await;
        eprintln!(
            "trial {trial_number}: success={}, refreshes={}, generation={}",
            attack.0, attack.1, attack.2
        );
        failed += usize::from(!attack.0 || attack.1 != 0 || attack.2 != 1);
    }
    assert_eq!(
        failed, 0,
        "{failed}/3 stale requests failed despite the completed login"
    );
}

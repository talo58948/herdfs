//! Round 70: a refresh racing a login whose opaque token strings are unchanged.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use axum::{
    Json, Router,
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, Semaphore};

const SHARED_ACCESS: &str = "shared-access";
const SHARED_REFRESH: &str = "shared-refresh";

async fn issuer(
    access: &'static str,
    started: Option<Arc<Notify>>,
    release: Option<Arc<Semaphore>>,
) -> String {
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
    let token = post(move || {
        let started = started.clone();
        let release = release.clone();
        async move {
            if let Some(started) = started {
                started.notify_one();
            }
            if let Some(release) = release {
                let _permit = release.acquire().await.unwrap();
            }
            Json(json!({
                "access_token": access,
                "refresh_token": SHARED_REFRESH,
                "expires_in": 3600
            }))
        }
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

fn expired(path: &Path, endpoint: &str) {
    std::fs::write(
        path,
        serde_json::to_vec(&json!({
            "access_token": SHARED_ACCESS,
            "refresh_token": SHARED_REFRESH,
            "expires_at": 0,
            "client_id": "old-client",
            "token_endpoint": endpoint
        }))
        .unwrap(),
    )
    .unwrap();
}

async fn callback(authorize: &str) {
    let authorize = reqwest::Url::parse(authorize).unwrap();
    let parameter = |name| {
        authorize
            .query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
            .unwrap()
    };
    let redirect = reqwest::Url::parse(&parameter("redirect_uri")).unwrap();
    let address = format!("{}:{}", redirect.host_str().unwrap(), redirect.port().unwrap());
    let mut stream = TcpStream::connect(address).await.unwrap();
    let target = format!("{}?code=new-code&state={}", redirect.path(), parameter("state"));
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.contains("200 OK"), "{response}");
}

async fn login(issuer: &str, token: &Path) {
    let mut login = Command::new(env!("CARGO_BIN_EXE_herdfs"));
    login
        .args(["login", "--issuer", issuer, "--client-id", "new-client"])
        .args(["--redirect-port", "0", "--token-file"])
        .arg(token)
        .env("PATH", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut login = login.spawn().unwrap();
    let mut output = BufReader::new(login.stdout.take().unwrap());
    let mut authorize = String::new();
    output.read_line(&mut authorize).await.unwrap();
    callback(authorize.trim()).await;
    assert!(login.wait().await.unwrap().success());
}

fn stale_refresh(token: &Path) -> Child {
    let mut refresh = Command::new(env!("CARGO_BIN_EXE_herdfs"));
    refresh
        .args(["whoami", "--authority", "http://127.0.0.1:9", "--token-file"])
        .arg(token)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    refresh.spawn().unwrap()
}

fn saved(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// A completed browser login is a new token generation even when an issuer reuses both opaque
/// token strings; an older refresh must not replace its client and endpoint metadata.
// contract: U1
#[tokio::test]
async fn completed_login_with_reused_tokens_wins_over_an_older_refresh() {
    let mut overwritten = 0;
    for trial in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        let started = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let old = issuer(
            "stale-refreshed-access",
            Some(Arc::clone(&started)),
            Some(Arc::clone(&release)),
        )
        .await;
        expired(&token, &format!("{old}/token"));
        let mut refresh = stale_refresh(&token);
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .unwrap();

        let new = issuer(SHARED_ACCESS, None, None).await;
        login(&new, &token).await;
        let logged_in = saved(&token);
        assert_eq!(logged_in["client_id"], "new-client", "login control {trial}");
        assert_eq!(logged_in["token_endpoint"], format!("{new}/token"));

        release.add_permits(1);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), refresh.wait())
            .await
            .unwrap();
        let after_refresh = saved(&token);
        overwritten += usize::from(after_refresh["client_id"] != "new-client");

        login(&new, &token).await;
        let control = saved(&token);
        assert_eq!(control["client_id"], "new-client", "uncontended control {trial}");
        assert_eq!(control["token_endpoint"], format!("{new}/token"));
    }
    assert_eq!(
        overwritten, 0,
        "older refresh replaced {overwritten}/3 completed logins"
    );
}

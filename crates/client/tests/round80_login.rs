//! Round 80: first login before the default token directory exists.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    body::Bytes,
    http::{HeaderMap, header::USER_AGENT},
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;

type Requests = Arc<Mutex<Vec<(String, String, String)>>>;

async fn issuer() -> (String, Requests) {
    let requests = Requests::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let discovery_base = base.clone();
    let discovery_requests = Arc::clone(&requests);
    let discovery = get(move |headers: HeaderMap| {
        let base = discovery_base.clone();
        let requests = Arc::clone(&discovery_requests);
        async move {
            record(&requests, "discovery", &headers, String::new());
            Json(json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token")
            }))
        }
    });
    let token_requests = Arc::clone(&requests);
    let token = post(move |headers: HeaderMap, body: Bytes| {
        let requests = Arc::clone(&token_requests);
        async move {
            record(
                &requests,
                "token",
                &headers,
                String::from_utf8(body.to_vec()).unwrap(),
            );
            Json(json!({
                "access_token": "access",
                "refresh_token": "refresh",
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
    (base, requests)
}

fn record(requests: &Requests, path: &str, headers: &HeaderMap, body: String) {
    let agent = headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    requests.lock().unwrap().push((path.to_owned(), agent, body));
}

async fn login(issuer: &str, home: &Path, resource: Option<&str>) -> (bool, String, reqwest::Url) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdfs"));
    command.args([
        "login",
        "--issuer",
        issuer,
        "--client-id",
        "client",
        "--redirect-port",
        "0",
    ]);
    if let Some(resource) = resource {
        command.args(["--resource", resource]);
    }
    let mut child = command
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
    let detail = format!(
        "callback={reply:?}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), detail, authorize)
}

fn parameter(url: &reqwest::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
}

/// A fresh default path must sign in just like the same path with its parent pre-created.
// contract: U1
#[tokio::test]
async fn first_login_creates_the_default_token_directory() {
    let (issuer, _) = issuer().await;
    let mut failed = 0;
    let mut failures = Vec::new();
    for trial in 0..3 {
        let fresh = tempfile::tempdir().unwrap();
        let (succeeded, detail, _) = login(&issuer, fresh.path(), None).await;
        let saved = fresh.path().join(".config/herdfs/token").exists();
        if !succeeded || !saved {
            failed += 1;
            failures.push(format!("fresh login {trial}: {detail}"));
        }

        let control = tempfile::tempdir().unwrap();
        fs::create_dir_all(control.path().join(".config/herdfs")).unwrap();
        let (succeeded, detail, _) = login(&issuer, control.path(), None).await;
        assert!(succeeded, "control login {trial}: {detail}");
        assert!(control.path().join(".config/herdfs/token").exists());
    }
    assert_eq!(failed, 0, "{failed}/3 fresh logins failed: {failures:#?}");
}

#[tokio::test]
async fn login_sends_resource_and_user_agent_only_as_requested() {
    // contract: U1, S5
    for resource in [Some("https://your-domain.example"), None] {
        let (issuer, requests) = issuer().await;
        let home = tempfile::tempdir().unwrap();
        let (succeeded, detail, authorize) = login(&issuer, home.path(), resource).await;
        assert!(succeeded, "{detail}");
        assert_eq!(parameter(&authorize, "resource").as_deref(), resource);

        let requests = requests.lock().unwrap();
        let expected_agent = format!("herdfs/{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.1 == expected_agent));
        let exchange = requests.iter().find(|request| request.0 == "token").unwrap();
        let form = reqwest::Url::parse(&format!("http://localhost/?{}", exchange.2)).unwrap();
        assert_eq!(parameter(&form, "resource").as_deref(), resource);

        let saved = fs::read(home.path().join(".config/herdfs/token")).unwrap();
        let saved: Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(saved.get("resource").and_then(Value::as_str), resource);
    }
}

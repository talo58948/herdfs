//! Round 66: browser login callbacks split across TCP reads.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    routing::{get, post},
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;

async fn callback(uri: &str, code: &str, state: &str) -> String {
    let url = reqwest::Url::parse(uri).unwrap();
    let address = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(format!("GET {}?code={code}", url.path()).as_bytes())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let rest = format!("&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(rest.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

async fn fragmented_trial(trial: usize) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let discovery = {
        let issuer = issuer.clone();
        get(move || {
            let issuer = issuer.clone();
            async move {
                Json(json!({
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token")
                }))
            }
        })
    };
    let token = {
        let received = Arc::clone(&received);
        post(move |body: Bytes| {
            let received = Arc::clone(&received);
            async move {
                received
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(body.to_vec()).unwrap());
                Json(json!({
                    "access_token": "access",
                    "refresh_token": "refresh",
                    "expires_in": 3600
                }))
            }
        })
    };
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/.well-known/openid-configuration", discovery)
                .route("/token", token),
        )
        .await
        .unwrap();
    });

    let dir = tempfile::tempdir().unwrap();
    let mut login = Command::new(env!("CARGO_BIN_EXE_herdfs"));
    login
        .args(["login", "--issuer", &issuer, "--client-id", "client"])
        .args(["--redirect-port", "0", "--token-file"])
        .arg(dir.path().join("token"))
        .env("PATH", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut login = login.spawn().unwrap();
    let mut output = BufReader::new(login.stdout.take().unwrap());
    let mut authorize = String::new();
    output.read_line(&mut authorize).await.unwrap();
    let authorize = reqwest::Url::parse(authorize.trim()).unwrap();
    let parameter = |name| {
        authorize
            .query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
            .unwrap()
    };
    let redirect = parameter("redirect_uri");
    let state = parameter("state");

    let fragmented = callback(&redirect, &format!("fragmented-{trial}"), &state).await;
    assert!(fragmented.contains("200 OK"), "fragmented trial {trial}");
    let status = tokio::time::timeout(Duration::from_secs(5), login.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        status.success(),
        "complete callback did not finish login, trial {trial}"
    );
    server.abort();

    let bodies = received.lock().unwrap();
    assert!(
        bodies
            .iter()
            .any(|body| body.contains(&format!("code=fragmented-{trial}"))),
        "fragmented code was not exchanged in trial {trial}"
    );
}

/// TCP may split a browser's callback request at any byte; login waits for its complete headers
/// before parsing the state and exchanging the code.
// contract: U1
#[tokio::test]
async fn login_accepts_fragmented_browser_callbacks() {
    for trial in 0..3 {
        fragmented_trial(trial).await;
    }
}

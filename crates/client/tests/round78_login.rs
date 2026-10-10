//! Round 78: malformed loopback traffic ahead of the real OIDC callback.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use axum::{
    Json, Router,
    routing::{get, post},
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdout, Command};

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

async fn login(issuer: &str, token: &Path) -> (Child, reqwest::Url, BufReader<ChildStdout>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdfs"));
    command
        .args(["login", "--issuer", issuer, "--client-id", "client"])
        .args(["--redirect-port", "0", "--token-file"])
        .arg(token)
        .env("PATH", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut authorize = String::new();
    stdout.read_line(&mut authorize).await.unwrap();
    (child, reqwest::Url::parse(authorize.trim()).unwrap(), stdout)
}

fn callback_target(authorize: &reqwest::Url) -> (String, String) {
    let parameter = |name| {
        authorize
            .query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
            .unwrap()
    };
    let redirect = reqwest::Url::parse(&parameter("redirect_uri")).unwrap();
    let address = format!("{}:{}", redirect.host_str().unwrap(), redirect.port().unwrap());
    let target = format!("{}?code=code&state={}", redirect.path(), parameter("state"));
    (address, target)
}

async fn callback(address: &str, target: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(address).await?;
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    Ok(reply)
}

async fn malformed(address: &str) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(b"GET /\xff HTTP/1.1\r\n\r\n").await.unwrap();
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).await.unwrap();
}

async fn control(issuer: &str, token: &Path) {
    let (mut login, authorize, _stdout) = login(issuer, token).await;
    let (address, target) = callback_target(&authorize);
    let reply = callback(&address, &target).await.unwrap();
    assert!(reply.contains("200 OK"), "{reply}");
    assert!(login.wait().await.unwrap().success());
}

/// Unrelated malformed loopback traffic must not terminate the listener before the browser sends
/// its valid state-and-code callback; an otherwise identical uncontended login is the control.
// contract: U1
#[tokio::test]
#[ignore = "ROUND78: malformed loopback traffic terminates browser login"]
async fn malformed_loopback_request_does_not_abort_browser_login() {
    let issuer = issuer().await;
    let mut aborted = 0;
    for trial in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        control(&issuer, &dir.path().join(format!("control-{trial}"))).await;

        let token = dir.path().join("token");
        let (mut login, authorize, _stdout) = login(&issuer, &token).await;
        let (address, target) = callback_target(&authorize);
        malformed(&address).await;
        let received = tokio::time::timeout(Duration::from_secs(1), callback(&address, &target)).await;
        let completed = received.is_ok_and(|reply| reply.is_ok_and(|reply| reply.contains("200 OK")));
        aborted += usize::from(!completed);
        let _ = login.kill().await;
        let _ = login.wait().await;
    }
    assert_eq!(aborted, 0, "{aborted}/3 valid browser callbacks were aborted");
}

//! The `herdfs-authority` binary starts with the flags the lab passes and serves health.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

use std::{
    net::TcpListener,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn the_binary_serves_health_with_the_documented_flags() {
    let dir = tempfile::tempdir().unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen = format!("127.0.0.1:{port}");
    let mut authority = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["--listen", &listen, "--db"])
        .arg(dir.path().join("authority.db"))
        .args([
            "--s3-endpoint",
            "http://127.0.0.1:9",
            "--s3-public-endpoint",
            "http://s3.lab:9",
        ])
        .args(["--s3-bucket", "herdfs", "--s3-region", "us-east-1"])
        .args([
            "--oidc-issuer",
            "http://127.0.0.1:9",
            "--oidc-audience",
            "herdfs",
        ])
        .envs([("AWS_ACCESS_KEY_ID", "key"), ("AWS_SECRET_ACCESS_KEY", "secret")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let health = format!("http://{listen}/v1/health");
    let started = Instant::now();
    let healthy = loop {
        let reply = reqwest::get(&health)
            .await
            .ok()
            .filter(|reply| reply.status().is_success());
        if reply.is_some() || started.elapsed() > Duration::from_secs(10) {
            break reply.is_some();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    authority.kill().unwrap();
    authority.wait().unwrap();
    assert!(healthy, "no health answer on {listen} within 10 s");
}

#[tokio::test]
async fn local_tokens_validate_expire_and_revoke() {
    // contract: S5
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen = format!("127.0.0.1:{port}");
    let mut authority = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["--listen", &listen, "--db"])
        .arg(&db)
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
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let base = format!("http://{listen}");
    wait_healthy(&base).await;

    let (id, token) = issue(&db, "alice", 1);
    let (status, reply) = rpc(&base, &token).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["principal"], "user:alice");

    let (_, expired) = issue(&db, "alice", 0);
    assert_eq!(rpc(&base, &expired).await.0, StatusCode::UNAUTHORIZED);

    let revoked = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["revoke-token", "--db"])
        .arg(&db)
        .arg(&id)
        .status()
        .unwrap();
    assert!(revoked.success());
    assert_eq!(rpc(&base, &token).await.0, StatusCode::UNAUTHORIZED);

    authority.kill().unwrap();
    authority.wait().unwrap();
}

fn issue(db: &std::path::Path, user: &str, days: u32) -> (String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["issue-token", "--db"])
        .arg(db)
        .args(["--user", user, "--days", &days.to_string()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let token = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    let id = String::from_utf8(output.stderr)
        .unwrap()
        .trim()
        .strip_prefix("token id: ")
        .unwrap()
        .to_owned();
    (id, token)
}

async fn rpc(base: &str, token: &str) -> (StatusCode, Value) {
    let reply = reqwest::Client::new()
        .post(format!("{base}/v1/rpc"))
        .bearer_auth(token)
        .json(&json!({"op": "whoami"}))
        .send()
        .await
        .unwrap();
    let status = reply.status();
    (status, reply.json().await.unwrap())
}

async fn wait_healthy(base: &str) {
    let started = Instant::now();
    loop {
        if reqwest::get(format!("{base}/v1/health"))
            .await
            .is_ok_and(|reply| reply.status().is_success())
        {
            return;
        }
        assert!(
            started.elapsed() <= Duration::from_secs(10),
            "authority did not become healthy"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn the_binary_refuses_a_grace_shorter_than_the_url_safety_bounds() {
    // contract: S4, S7
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        ["--grant-ttl", "60", "--gc-every", "1", "--gc-grace", "119"],
        ["--grant-ttl", "60", "--gc-every", "61", "--gc-grace", "120"],
    ];
    for flags in cases {
        let status = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
            .args(["--listen", "127.0.0.1:0", "--db"])
            .arg(dir.path().join("authority.db"))
            .args([
                "--s3-endpoint",
                "http://127.0.0.1:9",
                "--s3-public-endpoint",
                "http://s3.lab:9",
            ])
            .args(["--s3-bucket", "herdfs", "--s3-region", "us-east-1"])
            .args([
                "--oidc-issuer",
                "http://127.0.0.1:9",
                "--oidc-audience",
                "herdfs",
            ])
            .args(flags)
            .envs([("AWS_ACCESS_KEY_ID", "key"), ("AWS_SECRET_ACCESS_KEY", "secret")])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "accepted {flags:?}");
    }
}

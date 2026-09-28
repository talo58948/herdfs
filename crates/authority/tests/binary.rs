//! The `herdfs-authority` binary starts with the flags the lab passes and serves health.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

use std::{
    net::TcpListener,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

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

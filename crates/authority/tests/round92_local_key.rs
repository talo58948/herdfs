//! Round 92: persisted local-issuer keys exposed to other local users.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::SigningKey;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize)]
struct Stored {
    issuer: String,
    secret: String,
}

struct Authority(Child);

impl Authority {
    fn stop(mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Authority {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A restart must not trust a local signing key exposed for replacement by other users; a
/// freshly generated owner-only key and its normally issued token are the passing controls.
// contract: S5
#[tokio::test]
async fn exposed_local_key_cannot_authorize_forged_groups_after_restart() {
    for trial in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("authority.db");
        let (authority, listen, base) = start_new(&db).await;
        let valid = issue(&db);
        assert_eq!(rpc(&base, &valid).await.0, StatusCode::OK, "control {trial}");
        authority.stop();

        let key_path = dir.path().join("authority.db.local-issuer.json");
        let mut stored: Stored = serde_json::from_slice(&fs::read(&key_path).unwrap()).unwrap();
        let attacker = SigningKey::from_bytes(&rand::random());
        stored.secret = URL_SAFE_NO_PAD.encode(attacker.as_bytes());
        fs::write(&key_path, serde_json::to_vec(&stored).unwrap()).unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o666)).unwrap();

        let error = refusal(&db, &listen);
        assert!(error.contains(&key_path.display().to_string()), "{error}");
        assert!(error.contains("chmod 600"), "{error}");
        assert!(error.contains("delete it and reissue every token"), "{error}");
    }
}

fn authority_command(db: &Path, listen: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"));
    command
        .args(["--listen", listen, "--db"])
        .arg(db)
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
        .stderr(Stdio::null());
    command
}

async fn start_new(db: &Path) -> (Authority, String, String) {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen = format!("127.0.0.1:{port}");
    let (authority, base) = try_start(db, &listen).await.unwrap();
    (authority, listen, base)
}

async fn try_start(db: &Path, listen: &str) -> Option<(Authority, String)> {
    let mut authority = Authority(authority_command(db, listen).spawn().unwrap());
    let base = format!("http://{listen}");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if reqwest::get(format!("{base}/v1/health"))
            .await
            .is_ok_and(|reply| reply.status().is_success())
        {
            return Some((authority, base));
        }
        if authority.0.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn issue(db: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["issue-token", "--db"])
        .arg(db)
        .args(["--user", "alice", "--days", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn refusal(db: &Path, listen: &str) -> String {
    let mut authority = authority_command(db, listen)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = authority.try_wait().unwrap() {
            assert!(!status.success());
            let mut error = String::new();
            authority
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut error)
                .unwrap();
            return error;
        }
        if Instant::now() >= deadline {
            authority.kill().unwrap();
            authority.wait().unwrap();
            panic!("authority accepted exposed key");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn rpc(base: &str, token: &str) -> (StatusCode, Value) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/rpc"))
        .bearer_auth(token)
        .json(&json!({"op": "whoami"}))
        .send()
        .await
        .unwrap();
    (response.status(), response.json().await.unwrap())
}

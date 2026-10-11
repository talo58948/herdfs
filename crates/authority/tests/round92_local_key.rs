//! Round 92: persisted local-issuer keys exposed to other local users.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const PKCS8_PREFIX: &[u8] = &[
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

#[derive(Deserialize, Serialize)]
struct Stored {
    issuer: String,
    secret: String,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    aud: &'static str,
    sub: &'static str,
    exp: u64,
    jti: &'a str,
    groups: [&'static str; 1],
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
#[ignore = "ROUND92: an exposed persisted local key is trusted after restart"]
async fn exposed_local_key_cannot_authorize_forged_groups_after_restart() {
    let mut accepted = 0;
    for trial in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("authority.db");
        let (authority, listen, base) = start_new(&db).await;
        let (id, valid) = issue(&db);
        assert_eq!(rpc(&base, &valid).await.0, StatusCode::OK, "control {trial}");
        authority.stop();

        let key_path = dir.path().join("authority.db.local-issuer.json");
        let mut stored: Stored = serde_json::from_slice(&fs::read(&key_path).unwrap()).unwrap();
        let attacker = SigningKey::from_bytes(&rand::random());
        stored.secret = URL_SAFE_NO_PAD.encode(attacker.as_bytes());
        fs::write(&key_path, serde_json::to_vec(&stored).unwrap()).unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o666)).unwrap();

        if let Some((restarted, base)) = try_start(&db, &listen).await {
            let forged = sign(&stored.issuer, &id, &attacker);
            let (status, reply) = rpc(&base, &forged).await;
            let elevated = status == StatusCode::OK && reply["groups"] == json!(["admins"]);
            accepted += usize::from(elevated);
            eprintln!("trial {trial}: forged status={status}, reply={reply}");
            drop(restarted);
        }
    }
    assert_eq!(accepted, 0, "{accepted}/3 exposed keys authorized forged groups");
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

fn issue(db: &Path) -> (String, String) {
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
    let id = String::from_utf8(output.stderr)
        .unwrap()
        .trim()
        .strip_prefix("token id: ")
        .unwrap()
        .to_owned();
    (id, String::from_utf8(output.stdout).unwrap().trim().to_owned())
}

fn sign(issuer: &str, id: &str, key: &SigningKey) -> String {
    let claims = Claims {
        iss: issuer,
        aud: "herdfs",
        sub: "alice",
        exp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 86_400,
        jti: id,
        groups: ["admins"],
    };
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some("local".into());
    let mut der = Vec::from(PKCS8_PREFIX);
    der.extend_from_slice(key.as_bytes());
    jsonwebtoken::encode(&header, &claims, &EncodingKey::from_ed_der(&der)).unwrap()
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

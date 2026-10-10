//! Round 82: local-issuer algorithm, key-id, audience, and key-file boundaries.
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
use serde_json::json;

const PKCS8_PREFIX: &[u8] = &[
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

#[derive(Deserialize)]
struct Stored {
    issuer: String,
    secret: String,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    aud: &'a str,
    sub: &'static str,
    exp: u64,
    jti: &'a str,
}

struct Authority(Child);

impl Drop for Authority {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Local mode must accept only the configured signing key and audience, while its generated
/// private key remains owner-only; a normally issued token is the passing control.
// contract: S5
#[tokio::test]
async fn local_mode_rejects_unsigned_unknown_kid_and_wrong_audience_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let (authority, base) = start(&db).await;
    let key_path = dir.path().join("authority.db.local-issuer.json");
    assert_eq!(
        fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let stored: Stored = serde_json::from_slice(&fs::read(key_path).unwrap()).unwrap();
    let (id, valid) = issue(&db);

    let payload = valid.split('.').nth(1).unwrap();
    let none_header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","kid":"local","typ":"JWT"}"#);
    let unsigned = format!("{none_header}.{payload}.");
    let unknown = sign(&stored, &id, "herdfs", "external");
    let wrong_audience = sign(&stored, &id, "elsewhere", "local");

    for trial in 0..3 {
        for token in [&unsigned, &unknown, &wrong_audience] {
            assert_eq!(rpc(&base, token).await, StatusCode::UNAUTHORIZED, "trial {trial}");
        }
        assert_eq!(rpc(&base, &valid).await, StatusCode::OK, "control {trial}");
    }

    drop(authority);
}

fn sign(stored: &Stored, id: &str, audience: &str, kid: &str) -> String {
    let secret: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&stored.secret)
        .unwrap()
        .try_into()
        .unwrap();
    let claims = Claims {
        iss: &stored.issuer,
        aud: audience,
        sub: "alice",
        exp: now() + 86_400,
        jti: id,
    };
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(kid.into());
    let mut der = Vec::from(PKCS8_PREFIX);
    der.extend_from_slice(SigningKey::from_bytes(&secret).as_bytes());
    jsonwebtoken::encode(&header, &claims, &EncodingKey::from_ed_der(&der)).unwrap()
}

async fn start(db: &Path) -> (Authority, String) {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen = format!("127.0.0.1:{port}");
    let child = Authority(
        Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
            .args(["--listen", &listen, "--db"])
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
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let base = format!("http://{listen}");
    let started = Instant::now();
    loop {
        if reqwest::get(format!("{base}/v1/health"))
            .await
            .is_ok_and(|reply| reply.status().is_success())
        {
            return (child, base);
        }
        assert!(started.elapsed() < Duration::from_secs(10));
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

async fn rpc(base: &str, token: &str) -> StatusCode {
    reqwest::Client::new()
        .post(format!("{base}/v1/rpc"))
        .bearer_auth(token)
        .json(&json!({"op": "whoami"}))
        .send()
        .await
        .unwrap()
        .status()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

//! Built-in issuer acceptance and database-backed revocation.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::StatusCode;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    aud: &'static str,
    sub: &'a str,
    exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    jti: Option<&'a str>,
}

#[tokio::test]
async fn local_tokens_require_an_active_id_for_their_subject() {
    // contract: S5
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let (mut authority, base) = start(&db).await;
    let stored: Stored =
        serde_json::from_slice(&fs::read(dir.path().join("authority.db.local-issuer.json")).unwrap())
            .unwrap();
    let future = now() + 86_400;

    register(&db, "stale", "alice", 1);
    let (id, valid) = issue(&db, "alice");
    let connection = rusqlite::Connection::open(&db).unwrap();
    let stale: bool = connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM local_tokens WHERE id = 'stale')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!stale);
    let (status, reply) = rpc(&base, &valid).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["principal"], "user:alice");

    register(&db, "expired", "alice", future);
    let expired = sign(&stored, "alice", Some("expired"), 1, None);
    assert_eq!(rpc(&base, &expired).await.0, StatusCode::UNAUTHORIZED);

    let no_id = sign(&stored, "alice", None, future, None);
    assert_eq!(rpc(&base, &no_id).await.0, StatusCode::UNAUTHORIZED);

    let unknown = sign(&stored, "alice", Some("unknown"), future, None);
    assert_eq!(rpc(&base, &unknown).await.0, StatusCode::UNAUTHORIZED);

    let wrong_subject = sign(&stored, "bob", Some(&id), future, None);
    assert_eq!(rpc(&base, &wrong_subject).await.0, StatusCode::UNAUTHORIZED);

    let other = sign(
        &stored,
        "alice",
        Some(&id),
        future,
        Some(("https://other.invalid", SigningKey::from_bytes(&rand::random()))),
    );
    assert_eq!(rpc(&base, &other).await.0, StatusCode::UNAUTHORIZED);

    let revoked = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["revoke-token", "--db"])
        .arg(&db)
        .arg(&id)
        .status()
        .unwrap();
    assert!(revoked.success());
    assert_eq!(rpc(&base, &valid).await.0, StatusCode::UNAUTHORIZED);

    authority.kill().unwrap();
    authority.wait().unwrap();
}

#[tokio::test]
async fn local_issuer_rejects_ephemeral_and_changed_listen_addresses() {
    // contract: S5
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let ephemeral = authority_command(&db, "127.0.0.1:0").output().unwrap();
    assert!(!ephemeral.status.success());
    assert!(String::from_utf8_lossy(&ephemeral.stderr).contains("fixed, nonzero --listen port"));

    let (mut authority, base) = start(&db).await;
    authority.kill().unwrap();
    authority.wait().unwrap();
    let old_port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
    let port = loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if port != old_port {
            break port;
        }
    };
    let changed = authority_command(&db, &format!("127.0.0.1:{port}"))
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&changed.stderr);
    assert!(!changed.status.success());
    assert!(
        error.contains("changing --listen requires deleting the key file"),
        "{error}"
    );
    assert!(error.contains("reissuing every token"), "{error}");
}

#[test]
fn issue_rejects_zero_days_without_creating_a_database() {
    // contract: S5
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("mistyped.db");
    let output = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["issue-token", "--db"])
        .arg(&db)
        .args(["--user", "alice", "--days", "0"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!db.exists());

    let missing_key = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["issue-token", "--db"])
        .arg(&db)
        .args(["--user", "alice", "--days", "1"])
        .output()
        .unwrap();
    assert!(!missing_key.status.success());
    assert!(!db.exists());
}

async fn start(db: &Path) -> (Child, String) {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen = format!("127.0.0.1:{port}");
    let authority = authority_command(db, &listen)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let base = format!("http://{listen}");
    wait_healthy(&base).await;
    (authority, base)
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
        .envs([("AWS_ACCESS_KEY_ID", "key"), ("AWS_SECRET_ACCESS_KEY", "secret")]);
    command
}

fn issue(db: &Path, user: &str) -> (String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["issue-token", "--db"])
        .arg(db)
        .args(["--user", user, "--days", "1"])
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

fn register(db: &Path, id: &str, user: &str, expires: u64) {
    let connection = rusqlite::Connection::open(db).unwrap();
    connection
        .execute(
            "INSERT INTO local_tokens (id, user, expires_ms) VALUES (?1, ?2, ?3)",
            params![id, user, i64::try_from(expires * 1000).unwrap()],
        )
        .unwrap();
}

fn sign(
    stored: &Stored,
    sub: &str,
    jti: Option<&str>,
    exp: u64,
    other: Option<(&str, SigningKey)>,
) -> String {
    let secret: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&stored.secret)
        .unwrap()
        .try_into()
        .unwrap();
    let (issuer, key) = other.unwrap_or_else(|| (stored.issuer.as_str(), SigningKey::from_bytes(&secret)));
    let claims = Claims {
        iss: issuer,
        aud: "herdfs",
        sub,
        exp,
        jti,
    };
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some("local".into());
    let mut der = Vec::from(PKCS8_PREFIX);
    der.extend_from_slice(key.as_bytes());
    jsonwebtoken::encode(&header, &claims, &EncodingKey::from_ed_der(&der)).unwrap()
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

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

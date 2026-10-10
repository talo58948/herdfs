//! Round 86: every ID emitted by the local issuer must be accepted by its revocation CLI.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use herdfs_authority::{Authority, LocalIssuer};
use rusqlite::params;

fn revoke(db: &Path, id: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["revoke-token", "--db"])
        .arg(db)
        .arg(id)
        .output()
        .unwrap()
}

/// New IDs cannot look like flags, and a legacy base64url ID beginning with `-` remains revocable.
// contract: S5
#[test]
fn revoke_cli_accepts_every_issued_token_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let issuer = LocalIssuer::open(&db, SocketAddr::from(([127, 0, 0, 1], 7000))).unwrap();
    let mut authority = Authority::open(&db).unwrap();
    let current = authority
        .issue_local_token(&issuer, "alice", Duration::from_secs(86_400))
        .unwrap();
    assert!(!current.id.starts_with('-'));
    let output = revoke(&db, &current.id);
    assert!(
        output.status.success(),
        "current ID {:?}: {}",
        current.id,
        String::from_utf8_lossy(&output.stderr)
    );

    let issued = authority
        .issue_local_token(&issuer, "alice", Duration::from_secs(86_400))
        .unwrap();
    let legacy = "-legacy_base64url_id";
    drop(authority);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE local_tokens SET id = ?1 WHERE id = ?2",
            params![legacy, issued.id],
        )
        .unwrap();
    let output = revoke(&db, legacy);
    assert!(
        output.status.success(),
        "legacy ID {legacy:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

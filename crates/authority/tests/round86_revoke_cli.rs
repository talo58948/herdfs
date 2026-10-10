//! Round 86: every ID emitted by the local issuer must be accepted by its revocation CLI.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use herdfs_authority::{Authority, LocalIssuer};

fn revoke(db: &Path, id: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_herdfs-authority"))
        .args(["revoke-token", "--db"])
        .arg(db)
        .arg(id)
        .output()
        .unwrap()
}

/// Base64url IDs beginning with `-` are valid issuer output and must remain usable as the
/// positional `revoke-token` ID; an ordinary issued ID is the passing control.
// contract: S5
#[test]
#[ignore = "ROUND86: revoke-token parses issued IDs beginning with '-' as flags"]
fn revoke_cli_accepts_every_issued_token_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let issuer = LocalIssuer::open(&db, SocketAddr::from(([127, 0, 0, 1], 7000))).unwrap();
    let mut authority = Authority::open(&db).unwrap();
    let mut attacks = Vec::new();
    let mut control = None;
    for _ in 0..10_000 {
        let issued = authority
            .issue_local_token(&issuer, "alice", Duration::from_secs(86_400))
            .unwrap();
        if issued.id.starts_with('-') {
            attacks.push(issued.id);
        } else if control.is_none() {
            control = Some(issued.id);
        }
        if attacks.len() == 3 && control.is_some() {
            break;
        }
    }
    assert_eq!(attacks.len(), 3, "issuer did not produce three dash-prefixed IDs");

    let control = control.unwrap();
    let output = revoke(&db, &control);
    assert!(
        output.status.success(),
        "control ID {control:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut refused = 0;
    for (trial, id) in attacks.iter().enumerate() {
        let output = revoke(&db, id);
        refused += usize::from(!output.status.success());
        eprintln!(
            "trial {trial}: ID {id:?}, status={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(refused, 0, "{refused}/3 issued IDs could not be revoked");
}

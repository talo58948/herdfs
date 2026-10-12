//! Round 103: replacing a protected local-issuer key between its check and read.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, SigningKey, Verifier};
use herdfs_authority::{Authority, LocalIssuer};
use rustix::fs::{CWD, RenameFlags, renameat_with};
use serde_json::json;

fn key_path(db: &Path) -> std::path::PathBuf {
    db.with_file_name(format!(
        "{}.local-issuer.json",
        db.file_name().unwrap().to_string_lossy()
    ))
}

fn stored(path: &Path, issuer: &str, key: &SigningKey, mode: u32) {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "issuer": issuer,
            "secret": URL_SAFE_NO_PAD.encode(key.as_bytes()),
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn signed_by(token: &str, key: &SigningKey) -> bool {
    let (message, signature) = token.rsplit_once('.').unwrap();
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).unwrap()).unwrap();
    key.verifying_key().verify(message.as_bytes(), &signature).is_ok()
}

fn ordinary_key_control(trial: usize, listen: SocketAddr) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(format!("control-{trial}.db"));
    let issuer = LocalIssuer::open(&db, listen).unwrap();
    let issued = Authority::open(&db)
        .unwrap()
        .issue_local_token(&issuer, "control", Duration::from_secs(60))
        .unwrap();
    assert_eq!(issued.id.len(), 32);
}

fn raced_load(trial: usize) -> bool {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let db = dir.path().join("authority.db");
    let path = key_path(&db);
    let held = dir.path().join("protected-held");
    let link = dir.path().join("attacker-link");
    let attacker_file = dir.path().join("attacker.json");
    let listen = SocketAddr::from((Ipv4Addr::LOCALHOST, 20_000 + trial as u16));
    let expected_issuer = format!("http://{listen}");
    let protected = SigningKey::from_bytes(&[1 + trial as u8; 32]);
    let attacker = SigningKey::from_bytes(&[101 + trial as u8; 32]);

    stored(&path, "http://127.0.0.1:9", &protected, 0o600);
    stored(&attacker_file, &expected_issuer, &attacker, 0o666);
    symlink(&attacker_file, &link).unwrap();

    assert!(
        format!("{:#}", LocalIssuer::open(&db, listen).err().unwrap()).contains("--listen implies"),
        "protected regular-file control {trial}"
    );
    fs::rename(&path, &held).unwrap();
    fs::rename(&link, &path).unwrap();
    assert!(
        format!("{:#}", LocalIssuer::open(&db, listen).err().unwrap()).contains("is a symlink"),
        "stable symlink control {trial}"
    );
    fs::rename(&path, &link).unwrap();
    fs::rename(&held, &path).unwrap();

    let running = Arc::new(AtomicBool::new(true));
    let swapping = Arc::clone(&running);
    let swapper = thread::spawn(move || {
        while swapping.load(Ordering::Relaxed) {
            let _ = renameat_with(CWD, &path, CWD, &link, RenameFlags::EXCHANGE);
            thread::yield_now();
        }
    });

    let deadline = Instant::now() + Duration::from_secs(8);
    let loaded = loop {
        if let Ok(issuer) = LocalIssuer::open(&db, listen) {
            break Some(issuer);
        }
        if Instant::now() >= deadline {
            break None;
        }
    };
    running.store(false, Ordering::Relaxed);
    swapper.join().unwrap();

    let Some(issuer) = loaded else { return false };
    let issued = Authority::open(&db)
        .unwrap()
        .issue_local_token(&issuer, "forged", Duration::from_secs(60))
        .unwrap();
    assert!(
        signed_by(&issued.token, &attacker),
        "loaded another key in trial {trial}"
    );
    true
}

/// A restart must use the same protected regular file it checked, rather than an attacker-owned
/// symlink swapped into a writable parent; stable regular-file and symlink cases are controls.
// contract: S5
#[test]
fn local_issuer_does_not_follow_a_symlink_swapped_after_validation() {
    let mut forged = 0;
    for trial in 0..3 {
        let listen = SocketAddr::from((Ipv4Addr::LOCALHOST, 21_000 + trial as u16));
        ordinary_key_control(trial, listen);
        forged += usize::from(raced_load(trial));
    }
    assert_eq!(forged, 0, "loaded attacker signing keys in {forged}/3 trials");
}

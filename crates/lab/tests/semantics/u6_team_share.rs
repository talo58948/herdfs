//! U6: `/finance` is alice's alone, `/eng` is shared by alice and bob, and alice revokes bob
//! while he reads.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::thread;

use anyhow::Result;
use herdfs_core::ChunkRef;
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy, canary, sweep};

use crate::support::{self, VISIBLE_AFTER, errno, history, mkdir, noise, path, save, set_acl};

const SECRET: &str = "finance/q3.xlsx";
const DESIGN: &str = "eng/design.bin";
const DESIGN_LEN: usize = 16 << 20;
/// Chunks are at least 256 KiB, except a file's last, so each chunk holds a canary planted
/// this often and at the end.
const CANARY_EVERY: usize = 64 << 10;
/// What bob has read when alice revokes him.
const FIRST_PIECE: usize = 1 << 20;

/// `/finance` for alice, `/eng` for alice, who administers it, and bob. Neither keeps the root
/// grant `Lab::start` gives.
fn team() -> Result<Option<Lab>> {
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Wan, Strategy::Stream);
    let Some(lab) = support::start(config)? else {
        return Ok(None);
    };
    for dir in ["finance", "eng"] {
        mkdir(&lab, dir)?;
    }
    for user in ["user:alice", "user:bob"] {
        set_acl(&lab, "", user, None)?;
    }
    set_acl(&lab, "finance", "user:alice", Some(Perm::Write))?;
    set_acl(&lab, "eng", "user:alice", Some(Perm::Admin))?;
    set_acl(&lab, "eng", "user:bob", Some(Perm::Write))?;
    Ok(Some(lab))
}

#[test]
fn bob_gets_eacces_in_finance() -> Result<()> {
    // contract: S2
    let Some(lab) = team()? else {
        return Ok(());
    };
    let secret = canary();
    save(&lab.mount_path("a").join(SECRET), secret.as_bytes())?;
    assert_eq!(fs::read_to_string(lab.mount_path("a").join(SECRET))?, secret);

    let on_b = lab.mount_path("b");
    assert_eq!(errno(fs::read(on_b.join(SECRET))), Some(libc::EACCES));
    assert_eq!(errno(fs::read_dir(on_b.join("finance"))), Some(libc::EACCES));
    let hits = sweep(&[lab.client_dir("b")], &[secret])?;
    assert!(hits.is_empty(), "{hits:?}");
    Ok(())
}

#[test]
fn no_version_after_revocation_reaches_bob() -> Result<()> {
    // contract: S2, S4
    let Some(lab) = team()? else {
        return Ok(());
    };
    let (on_a, on_b) = (lab.mount_path("a").join(DESIGN), lab.mount_path("b").join(DESIGN));
    let before = noise(DESIGN_LEN);
    save(&on_a, &before)?;
    thread::sleep(VISIBLE_AFTER);
    let mut reading = File::open(&on_b)?;
    let mut read = vec![0; FIRST_PIECE];
    reading.read_exact(&mut read)?;

    let revoke = Request::SetAcl {
        space: SPACE.into(),
        path: path("eng")?,
        principal: "user:bob".into(),
        perm: None,
    };
    lab.rpc("alice", &revoke)?;
    let secret = canary();
    save(&on_a, &planted(&secret))?;
    assert_eq!(errno(File::open(&on_b)), Some(libc::EACCES));
    // Bob's open handle keeps its version; the revocation may or may not cut his read short.
    let _ = reading.read_to_end(&mut read);
    assert!(before.starts_with(&read), "bob read bytes of another version");
    thread::sleep(VISIBLE_AFTER);
    assert_eq!(errno(File::open(&on_b)), Some(libc::EACCES));

    let [old, new] = &history(&lab, "alice", DESIGN)?[..] else {
        panic!("two saves make two versions");
    };
    let cached = cached_chunks(&lab, "b")?;
    let is_cached = |chunk: &ChunkRef| cached.contains(&chunk.id.to_string());
    assert!(old.chunks.iter().any(is_cached), "bob caches what he read");
    assert!(!new.chunks.iter().any(is_cached), "bob caches a later version");
    let hits = sweep(&[lab.client_dir("b")], &[secret])?;
    assert!(hits.is_empty(), "{hits:?}");
    Ok(())
}

/// A new design, with `secret` planted in every chunk.
fn planted(secret: &str) -> Vec<u8> {
    let mut bytes = noise(DESIGN_LEN);
    let ends = (CANARY_EVERY..DESIGN_LEN)
        .step_by(CANARY_EVERY)
        .chain([DESIGN_LEN]);
    for end in ends {
        bytes[end - secret.len()..end].copy_from_slice(secret.as_bytes());
    }
    bytes
}

/// Chunk ids in client `name`'s cache, where the lab keeps it.
fn cached_chunks(lab: &Lab, name: &str) -> Result<HashSet<String>> {
    fs::read_dir(lab.client_dir(name).join("cache"))?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect()
}

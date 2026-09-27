//! Ring R4: the attacker is root on bob's client. alice keeps secrets under `/finance`, and every
//! chunk of them carries a canary; bob may write only `/eng`. The attacker holds bob's token,
//! cache, journal and log, and reaches the authority and the store over bob's links. After every
//! attack, bob's directory, his readable view of the mount and every answer the attacker got are
//! swept for canaries. Skips until the binaries are built.

mod access;
mod attacker;
mod store;
mod uploads;

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use herdfs_core::proto::{EntryId, Perm, Request, Response, Version, VersionId};
use herdfs_core::{ChunkRef, SpacePath};
use herdfs_lab::{ADMIN, Hit, Lab, LabConfig, Profile, SPACE, Strategy, canary, sweep};

use crate::attacker::{Attacker, Reply};
use serde_json::Value;

/// Client and user names are the same.
const ALICE: &str = "alice";
const BOB: &str = "bob";
const SECRET_LEN: usize = 3 << 20;
/// Every run of this many bytes of a secret starts with its canary.
const CANARY_EVERY: usize = 1024;
/// Several chunks, so attacks can mix them up.
const PLAN_LEN: usize = 6 << 20;

/// A committed file and the version the attacks aim at.
struct Target {
    path: SpacePath,
    version: Version,
    bytes: Vec<u8>,
}

impl Target {
    /// The bytes of chunk `index` of the version.
    fn chunk(&self, index: usize) -> &[u8] {
        let chunks = &self.version.chunks;
        let start: usize = chunks[..index].iter().map(|chunk| chunk.len as usize).sum();
        &self.bytes[start..start + chunks[index].len as usize]
    }

    fn chunk_indices(&self) -> Vec<u32> {
        (0..self.version.chunks.len() as u32).collect()
    }
}

/// What every attack starts from.
struct Scene {
    lab: Lab,
    /// alice's `/finance/q3.xlsx`, several chunks.
    secret: Target,
    /// alice's `/finance/<canary>.txt`, one chunk, whose name is as secret as its bytes.
    small_secret: Target,
    /// alice's `/eng/plan.txt`, which bob may read.
    plan: Target,
    canaries: Vec<String>,
    attacker: Attacker,
}

impl Scene {
    /// alice owns the space and bob keeps write on `/eng` and read on `/.agents` only. bob's mount
    /// starts before his access narrows, because the client cannot mount without read on the
    /// root (see `bob_mounts_with_access_below_the_root_only`).
    fn start() -> Result<Option<Self>> {
        if let Some(reason) = herdfs_lab::missing_binaries() {
            eprintln!("skipped: {reason}");
            return Ok(None);
        }
        let lab = Lab::start(
            LabConfig::default()
                .client(ALICE, ALICE, &[], Profile::Lan, Strategy::Stream)
                .client(BOB, BOB, &[], Profile::Lan, Strategy::Stream),
        )?;
        for dir in ["finance", "eng", ".agents/C"] {
            fs::create_dir_all(lab.mount_path(ALICE).join(dir))?;
        }
        let acl = [
            ("", "user:alice", Some(Perm::Admin)),
            ("", "user:bob", None),
            ("eng", "user:bob", Some(Perm::Write)),
            (".agents", "user:bob", Some(Perm::Read)),
        ];
        for (path, principal, perm) in acl {
            set_acl(&lab, ADMIN, path, principal, perm)?;
        }
        let canaries = vec![canary(), canary()];
        let secret = write_as_alice(&lab, "finance/q3.xlsx", secret_bytes(&canaries[0], SECRET_LEN))?;
        let named = format!("finance/{}.txt", canaries[1]);
        let small_secret = write_as_alice(&lab, &named, secret_bytes(&canaries[1], 4096))?;
        ensure!(
            secret.version.chunks.len() >= 2 && small_secret.version.chunks.len() == 1,
            "unexpected chunking of the secrets"
        );
        for index in 0..secret.version.chunks.len() {
            let carries = contains(secret.chunk(index), canaries[0].as_bytes());
            ensure!(carries, "chunk {index} of the secret carries no canary");
        }
        // The oracle works: it finds every canary in alice's view.
        let seen = sweep(&[&lab.mount_path(ALICE).join("finance")], &canaries)?;
        let found = |canary: &String| seen.iter().any(|hit| &hit.canary == canary);
        ensure!(canaries.iter().all(found), "the sweep found only {seen:?}");
        let plan = write_as_alice(&lab, "eng/plan.txt", random_bytes(PLAN_LEN))?;
        ensure!(
            plan.version.chunks.len() >= 3,
            "the plan needs three chunks to tamper with"
        );
        let attacker = Attacker::new(&lab)?;
        Ok(Some(Self {
            lab,
            secret,
            small_secret,
            plan,
            canaries,
            attacker,
        }))
    }

    fn bob_mount(&self, path: &str) -> PathBuf {
        self.lab.mount_path(BOB).join(path)
    }

    /// Canaries in bob's directory, the attacker's loot and `views` of bob's mount.
    fn leaks(&self, views: &[&str]) -> Result<Vec<Hit>> {
        let views: Vec<PathBuf> = views.iter().map(|view| self.bob_mount(view)).collect();
        let mut paths = vec![self.lab.client_dir(BOB), self.attacker.loot.path()];
        paths.extend(views.iter().map(PathBuf::as_path));
        Ok(sweep(&paths, &self.canaries)?)
    }

    /// Fails on any canary in bob's footprint, including everything he can read on his mount.
    fn assert_no_leak(&self) -> Result<()> {
        assert_eq!(self.leaks(&["eng", ".agents"])?, [], "canaries reached bob");
        Ok(())
    }
}

/// Writes `bytes` to `path` through alice's mount and returns the version committed.
fn write_as_alice(lab: &Lab, path: &str, bytes: Vec<u8>) -> Result<Target> {
    let mut file = File::create(lab.mount_path(ALICE).join(path))?;
    file.write_all(&bytes)?;
    // fsync returns once the authority has committed the version.
    file.sync_all()?;
    drop(file);
    let path = SpacePath::try_from(path)?;
    let open = Request::Open {
        space: SPACE.into(),
        path: path.clone(),
    };
    let Response::Opened {
        version: Some(version),
        ..
    } = lab.rpc(ALICE, &open)?
    else {
        bail!("{path} has no version");
    };
    Ok(Target { path, version, bytes })
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    rand::fill(&mut bytes[..]);
    bytes
}

/// Random bytes with `canary` at the start of every `CANARY_EVERY` bytes and at the end, so
/// every chunk of them carries it.
fn secret_bytes(canary: &str, len: usize) -> Vec<u8> {
    let mut bytes = random_bytes(len);
    for run in bytes.chunks_mut(CANARY_EVERY) {
        run[..canary.len()].copy_from_slice(canary.as_bytes());
    }
    bytes[len - canary.len()..].copy_from_slice(canary.as_bytes());
    bytes
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// An unguarded commit with a fresh request id.
fn commit(entry: EntryId, base: Option<VersionId>, chunks: Vec<ChunkRef>) -> Request {
    Request::Commit {
        space: SPACE.into(),
        entry,
        request_id: format!("{:032x}", rand::random::<u128>()),
        base,
        guarded: false,
        chunks,
    }
}

/// A JSON request that names `path` verbatim, valid or not.
fn at_path(op: &str, path: &str) -> Value {
    serde_json::json!({"op": op, "space": SPACE, "path": path})
}

/// Sets `principal`'s perm on `path`, as `user`.
fn set_acl(lab: &Lab, user: &str, path: &str, principal: &str, perm: Option<Perm>) -> Result<()> {
    let (space, path, principal) = (SPACE.into(), SpacePath::try_from(path)?, principal.into());
    let request = Request::SetAcl {
        space,
        path,
        principal,
        perm,
    };
    lab.rpc(user, &request)?;
    Ok(())
}

/// Fails unless `got` is exactly `expected`, without printing megabytes of either.
fn assert_same(got: &[u8], expected: &[u8], what: &str) {
    let (got_len, expected_len) = (got.len(), expected.len());
    assert!(
        got == expected,
        "{what}: {got_len} bytes, not the {expected_len} expected"
    );
}

/// What `errno` gives for a refusal.
const EACCES: Option<i32> = Some(libc::EACCES);

/// The errno of an operation that must fail; `None` if it succeeded.
fn errno<T>(result: io::Result<T>) -> Option<i32> {
    result.err().and_then(|error| error.raw_os_error())
}

// contract: S3
#[test]
#[ignore = "BUG: the client stats the space root when it mounts, so a principal without read on the root cannot mount"]
fn bob_mounts_with_access_below_the_root_only() -> Result<()> {
    let Some(Scene { mut lab, plan, .. }) = Scene::start()? else {
        return Ok(());
    };
    lab.restart_client(BOB).context("mounting as bob")?;
    assert_same(
        &fs::read(lab.mount_path(BOB).join("eng/plan.txt"))?,
        &plan.bytes,
        "bob's plan",
    );
    Ok(())
}

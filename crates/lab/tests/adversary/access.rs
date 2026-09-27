//! Attacks the authority must refuse: requests for alice's files and names, bad tokens, paths out
//! of `/eng`, forks, and sessions on another host.

use std::fs;
use std::os::unix::fs::symlink;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use herdfs_core::SpacePath;
use herdfs_core::proto::{EntryId, Perm, Request, Response};
use herdfs_lab::{BadToken, SPACE};
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::{ALICE, BOB, EACCES, Scene, at_path, commit, errno, set_acl};

// contract: S2, S3
#[test]
fn bob_gets_no_grant_and_no_metadata_for_alices_files() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    let (secret, plan) = (&scene.secret, &scene.plan);
    let grant = |path: &SpacePath| Request::Grant {
        space: SPACE.into(),
        path: path.clone(),
        version: secret.version.id,
        chunks: secret.chunk_indices(),
    };
    assert_eq!(
        scene.attacker.call(&grant(&secret.path))?.status,
        StatusCode::FORBIDDEN
    );
    let secret_path = secret.path.as_str();
    let named = [
        ("open", secret_path),
        ("stat", secret_path),
        ("history", secret_path),
    ];
    let listed = [("list", secret_path), ("list", "finance"), ("list", "")];
    for (op, path) in named.into_iter().chain(listed) {
        let reply = scene.attacker.rpc(&at_path(op, path))?;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{op} /{path}");
    }
    // A path bob may read lends its permission to none of alice's versions.
    let restore = Request::Restore {
        space: SPACE.into(),
        path: plan.path.clone(),
        version: secret.version.id,
    };
    for request in [grant(&plan.path), restore] {
        let reply = scene.attacker.call(&request)?;
        assert!(!reply.status.is_success(), "{request:?}: {}", reply.status);
    }
    scene.assert_no_leak()
}

/// Entry ids are not secret, but the names of alice's entries are: bob may not list `/finance`.
// contract: S3
#[test]
fn probing_entry_ids_reveals_no_names() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    // Ids count up, so every entry lies at or below the newest one's.
    for id in 1..=scene.plan.version.entry.0 {
        scene.attacker.call(&commit(EntryId(id), None, Vec::new()))?;
    }
    scene.assert_no_leak()
}

/// A token signed with HS256 and keyed with the issuer's published Ed25519 key: a server that let
/// the header choose the algorithm would take the public key for a shared secret.
fn confused_token(scene: &Scene) -> Result<String> {
    let issuer = scene.lab.issuer_url();
    let discovery_url = format!("{issuer}/.well-known/openid-configuration");
    let discovery: Value = reqwest::blocking::get(discovery_url)?.json()?;
    let jwks_url = discovery["jwks_uri"]
        .as_str()
        .context("discovery names the JWKS")?;
    let jwks: Value = reqwest::blocking::get(jwks_url)?.json()?;
    let key = &jwks["keys"][0];
    let mut header = Header::new(Algorithm::HS256);
    header.kid = key["kid"].as_str().map(String::from);
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let claims = json!({"iss": issuer, "sub": BOB, "aud": "herdfs", "iat": now, "exp": now + 3600});
    let public = key["x"].as_str().context("the issuer publishes an Ed25519 key")?;
    let secret = EncodingKey::from_secret(public.as_bytes());
    Ok(jsonwebtoken::encode(&header, &claims, &secret)?)
}

// contract: S5
#[test]
fn bad_tokens_are_refused_with_401() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    let kinds = [
        BadToken::Expired,
        BadToken::WrongAudience,
        BadToken::UnknownKey,
        BadToken::AlgNone,
    ];
    let bad = kinds.map(|kind| (format!("{kind:?}"), Some(scene.lab.bad_token(BOB, kind))));
    let crafted = [
        (
            "HS256 keyed with the public key".into(),
            Some(confused_token(&scene)?),
        ),
        ("no token".into(), None),
        ("not a JWT".into(), Some("not-a-jwt".into())),
    ];
    let (plan, chunk) = (&scene.plan, scene.plan.version.chunks[0]);
    // A grant bob could get with his real token, so only the token can be the reason.
    let grant = json!({
        "op": "grant", "space": SPACE, "path": plan.path, "version": plan.version.id, "chunks": [0],
    });
    for (name, token) in bad.iter().chain(&crafted) {
        let token = token.as_deref();
        for request in [json!({"op": "whoami"}), grant.clone()] {
            let reply = scene.attacker.rpc_with(token, &request)?;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{name}: {request}");
        }
        let upload = scene.attacker.upload(token, &chunk, plan.chunk(0))?;
        assert_eq!(upload.status, StatusCode::UNAUTHORIZED, "{name}: upload");
    }
    assert!(
        scene.attacker.rpc(&grant)?.status.is_success(),
        "bob's own token works"
    );
    scene.assert_no_leak()
}

// contract: S2
#[test]
fn dot_dot_symlinks_and_renames_do_not_reach_finance() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    // The protocol cannot express these paths; the attacker sends them anyway.
    let escapes = [
        "eng/../finance/q3.xlsx",
        "eng/./../finance",
        "/finance/q3.xlsx",
        "eng//..",
        "..",
    ];
    for escape in escapes {
        let grant = json!({"op": "grant", "space": SPACE, "path": escape, "version": 1, "chunks": [0]});
        let by_path = ["open", "stat", "history", "list", "remove"].map(|op| at_path(op, escape));
        for request in by_path.iter().chain([&grant]) {
            let reply = scene.attacker.rpc(request)?;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{request}");
        }
    }
    // Through the mount the kernel resolves `..` and symlinks, and the lookup of /finance fails.
    assert_eq!(errno(fs::read(scene.bob_mount("eng/../finance/q3.xlsx"))), EACCES);
    let link = scene.bob_mount("eng/q3-link");
    symlink("../finance/q3.xlsx", &link)?;
    assert_eq!(errno(fs::read(&link)), EACCES);
    let planted = json!({"op": "symlink", "space": SPACE, "path": "finance/link", "target": "../eng"});
    assert_eq!(scene.attacker.rpc(&planted)?.status, StatusCode::FORBIDDEN);

    // A rename needs write on both sides, so nothing crosses the boundary either way.
    let crossings = [
        ("finance/q3.xlsx", "eng/q3.xlsx"),
        ("finance", "eng/finance"),
        ("eng/plan.txt", "finance/q3.xlsx"),
        ("eng", "finance/eng"),
        ("eng", "eng2"),
    ];
    for (from, to) in crossings {
        let rename = json!({"op": "rename", "space": SPACE, "from": from, "to": to});
        let reply = scene.attacker.rpc(&rename)?;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{from} -> {to}");
    }
    let moved = fs::rename(scene.bob_mount("finance/q3.xlsx"), scene.bob_mount("eng/q3.xlsx"));
    assert_eq!(errno(moved), EACCES);

    let open = Request::Open {
        space: SPACE.into(),
        path: scene.secret.path.clone(),
    };
    let Response::Opened { entry, .. } = scene.lab.rpc(ALICE, &open)? else {
        bail!("open answers with the entry");
    };
    assert_eq!(
        entry.head,
        Some(scene.secret.version.id),
        "alice's file is untouched"
    );
    scene.assert_no_leak()
}

// contract: C13, S2
#[test]
fn only_an_admin_of_the_whole_space_may_fork_it() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    let fork = json!({"op": "fork", "space": SPACE, "name": "bobs-fork"});
    assert_eq!(scene.attacker.rpc(&fork)?.status, StatusCode::FORBIDDEN);
    // Admin of `/eng` is not enough: the fork would copy `/finance` too.
    set_acl(&scene.lab, ALICE, "eng", "user:bob", Some(Perm::Admin))?;
    assert_eq!(scene.attacker.rpc(&fork)?.status, StatusCode::FORBIDDEN);
    let stat = Request::Stat {
        space: "bobs-fork".into(),
        path: SpacePath::root(),
    };
    assert!(scene.lab.rpc(ALICE, &stat).is_err(), "no fork exists");
    scene.assert_no_leak()
}

// contract: A1
#[test]
fn bob_cannot_start_a_session_on_another_host() -> Result<()> {
    let Some(scene) = Scene::start()? else {
        return Ok(());
    };
    let host = scene.bob_mount(".agents/C");
    assert!(host.is_dir(), "bob may look into /.agents");
    assert_eq!(errno(fs::create_dir(host.join("t1"))), EACCES);
    assert_eq!(errno(fs::write(host.join("prompt"), "rm -rf /")), EACCES);
    for op in ["mkdir", "create"] {
        let request = json!({"op": op, "space": SPACE, "path": ".agents/C/t1", "mode": 0o755});
        assert_eq!(
            scene.attacker.rpc(&request)?.status,
            StatusCode::FORBIDDEN,
            "{op}"
        );
    }
    let list = Request::List {
        space: SPACE.into(),
        path: SpacePath::try_from(".agents/C")?,
    };
    let Response::Entries { entries } = scene.lab.rpc(ALICE, &list)? else {
        bail!("list answers with entries");
    };
    assert!(entries.is_empty(), "no session on host C: {entries:?}");
    scene.assert_no_leak()
}

//! Red team, round 34: collection state across forks, restores, replays, and a recommit between
//! sweeps. The probe held and remains as a regression test.
#![allow(clippy::unwrap_used, reason = "test code")]

mod http;

use std::time::Duration;

use axum::http::StatusCode;
use herdfs_authority::Policy;
use herdfs_core::ChunkId;
use http::{Lab, now_ms};
use serde_json::{Value, json};

const GRACE_MS: i64 = 12_000;
const POLICY: Policy = Policy {
    versions: 1,
    max_age_ms: 0,
    grace_ms: GRACE_MS,
};

fn key(bytes: &[u8]) -> String {
    format!("herdfs/chunks/{}", ChunkId::of(bytes))
}

async fn create(lab: &Lab, token: &str, space: &str, parent: &Value, name: &str) -> Value {
    let request = json!({"op": "create", "space": space, "parent": parent, "name": name, "mode": 420});
    let (status, reply) = lab.rpc(token, &request).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    reply["entry"]["id"].clone()
}

async fn stat(lab: &Lab, token: &str, space: &str, path: &str) -> Value {
    let (status, reply) = lab
        .rpc(token, &json!({"op": "stat", "space": space, "path": path}))
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    reply["entry"].clone()
}

async fn upload(lab: &Lab, token: &str, space: &str, bytes: &[u8]) {
    let (status, reply) = lab.upload(token, space, ChunkId::of(bytes), bytes.to_vec()).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
}

async fn commit(
    lab: &Lab,
    token: &str,
    space: &str,
    entry: &Value,
    request_id: &str,
    base: Option<&Value>,
    bytes: &[u8],
) -> Value {
    let chunk = json!({"id": ChunkId::of(bytes).to_string(), "len": bytes.len()});
    let request = json!({"op": "commit", "space": space, "entry": entry,
        "request_id": request_id, "base": base, "guarded": false, "chunks": [chunk]});
    let (status, reply) = lab.rpc(token, &request).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    reply["version"].clone()
}

async fn restore(lab: &Lab, token: &str, space: &str, version: &Value) -> Value {
    let request = json!({"op": "restore", "space": space, "path": "f", "version": version});
    let (status, reply) = lab.rpc(token, &request).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    reply["version"].clone()
}

/// A chunk can be copied by a fork and restored, then leave every retained version. Replaying its
/// old commit must not revive it; a later real re-upload and commit must reset its collection age,
/// while an otherwise identical dead control is erased.
// contract: C13, C14, C15, S4, S7
#[tokio::test]
async fn only_a_real_recommit_revives_a_collected_fork_chunk() {
    let lab = Lab::start_with_ttl(Duration::from_secs(5)).await;
    let alice = lab.user("alice");
    let old = b"round 34 restored fork chunk";
    let control = b"round 34 dead control";
    let team_head = b"round 34 team head";
    let copy_head = b"round 34 copy head";

    let team_root = lab.create_space(&alice, "team").await;
    let team_f = create(&lab, &alice, "team", &team_root, "f").await;
    let team_g = create(&lab, &alice, "team", &team_root, "g").await;
    upload(&lab, &alice, "team", old).await;
    upload(&lab, &alice, "team", control).await;
    let old_team = commit(&lab, &alice, "team", &team_f, "old-team", None, old).await;
    commit(&lab, &alice, "team", &team_g, "control-team", None, control).await;

    let (status, forked) = lab
        .rpc(&alice, &json!({"op": "fork", "space": "team", "name": "copy"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{forked}");
    let copy_f = stat(&lab, &alice, "copy", "f").await;
    let copy_g = stat(&lab, &alice, "copy", "g").await;
    let copied_old = copy_f["head"].clone();

    upload(&lab, &alice, "team", team_head).await;
    let team_current = commit(
        &lab,
        &alice,
        "team",
        &team_f,
        "team-f-head",
        Some(&old_team["id"]),
        team_head,
    )
    .await;
    commit(&lab, &alice, "team", &team_g, "team-g-head", None, team_head).await;
    upload(&lab, &alice, "copy", copy_head).await;
    commit(
        &lab,
        &alice,
        "copy",
        &copy_f["id"],
        "copy-f-first",
        None,
        copy_head,
    )
    .await;
    let restored = restore(&lab, &alice, "copy", &copied_old).await;
    let copy_current = commit(
        &lab,
        &alice,
        "copy",
        &copy_f["id"],
        "copy-f-head",
        Some(&restored["id"]),
        copy_head,
    )
    .await;
    commit(
        &lab,
        &alice,
        "copy",
        &copy_g["id"],
        "copy-g-head",
        None,
        copy_head,
    )
    .await;

    let first = now_ms();
    lab.server.sweep(first, &POLICY).await.unwrap();
    assert!(lab.objects.lock().unwrap().contains_key(&key(old)));
    assert!(lab.objects.lock().unwrap().contains_key(&key(control)));

    let replay = json!({"op": "commit", "space": "team", "entry": team_f,
        "request_id": "old-team", "base": null, "guarded": false, "chunks": []});
    let (status, replayed) = lab.rpc(&alice, &replay).await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed["version"]["id"], team_current["id"]);
    let (status, hidden) = lab
        .rpc(
            &alice,
            &json!({"op": "restore", "space": "copy", "path": "f", "version": copied_old}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{hidden}");

    upload(&lab, &alice, "copy", old).await;
    let revived = commit(
        &lab,
        &alice,
        "copy",
        &copy_f["id"],
        "revived",
        Some(&copy_current["id"]),
        old,
    )
    .await;
    let grant = json!({"op": "grant", "space": "copy", "entry": copy_f["id"],
        "version": revived["id"], "chunks": [0]});
    assert_eq!(lab.rpc(&alice, &grant).await.0, StatusCode::OK);
    assert_eq!(
        lab.rpc(&alice, &json!({"op": "delete_space", "space": "copy"}))
            .await
            .0,
        StatusCode::OK
    );

    lab.server.sweep(first + GRACE_MS + 1, &POLICY).await.unwrap();
    let objects = lab.objects.lock().unwrap();
    assert!(
        !objects.contains_key(&key(control)),
        "dead control survived its grace"
    );
    assert!(
        objects.contains_key(&key(old)),
        "recommitted chunk was deleted during its grant"
    );
}

/// Overlapping sweep calls may race their listings and aging transactions, but both must agree
/// on the global retained set: an expired dead chunk goes and a retained control stays.
// contract: C15, S7
#[tokio::test]
async fn concurrent_sweeps_keep_the_same_global_live_set() {
    let lab = Lab::start_with_ttl(Duration::from_secs(5)).await;
    let alice = lab.user("alice");
    let dead = b"round 34 concurrent dead";
    let live = b"round 34 concurrent live";

    let dead_root = lab.create_space(&alice, "dead").await;
    let dead_file = create(&lab, &alice, "dead", &dead_root, "f").await;
    upload(&lab, &alice, "dead", dead).await;
    commit(&lab, &alice, "dead", &dead_file, "dead", None, dead).await;
    assert_eq!(
        lab.rpc(&alice, &json!({"op": "delete_space", "space": "dead"}))
            .await
            .0,
        StatusCode::OK
    );

    let live_root = lab.create_space(&alice, "live").await;
    let live_file = create(&lab, &alice, "live", &live_root, "f").await;
    upload(&lab, &alice, "live", live).await;
    commit(&lab, &alice, "live", &live_file, "live", None, live).await;

    let first = now_ms();
    lab.server.sweep(first, &POLICY).await.unwrap();
    let (left, right) = tokio::join!(
        lab.server.sweep(first + GRACE_MS + 1, &POLICY),
        lab.server.sweep(first + GRACE_MS + 2, &POLICY),
    );
    left.unwrap();
    right.unwrap();

    let objects = lab.objects.lock().unwrap();
    assert!(!objects.contains_key(&key(dead)), "expired dead chunk survived");
    assert!(objects.contains_key(&key(live)), "retained chunk was collected");
}

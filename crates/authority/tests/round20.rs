//! Red team, round 20 probes: write access revoked while an upload's bytes are in flight, after
//! its check and before its record (held; they stay as regression tests), and a sweep racing an
//! outstanding URL for a version that has just left retention (broken in round 20, now fixed).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod http;

use std::{sync::Arc, time::Duration};

use axum::http::StatusCode;
use herdfs_authority::Policy;
use herdfs_core::ChunkId;
use http::{Lab, now_ms};
use serde_json::{Value, json};

const NOTHING_KEPT: Policy = Policy {
    versions: 1,
    max_age_ms: 0,
    grace_ms: 0,
};

fn key(bytes: &[u8]) -> String {
    format!("herdfs/chunks/{}", ChunkId::of(bytes))
}

async fn set_acl(lab: &Lab, admin: &str, principal: &str, perm: Option<&str>) {
    let body = json!({"op": "set_acl", "space": "team", "path": "", "principal": principal, "perm": perm});
    let (status, reply) = lab.rpc(admin, &body).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
}

/// Holds the fake bucket's write permit until the returned permit is dropped.
async fn stall_store(lab: &Lab) -> tokio::sync::OwnedSemaphorePermit {
    Arc::clone(&lab.store_writes).acquire_owned().await.unwrap()
}

/// Bob's write is revoked while his upload's bytes are being stored. Whatever the answer, the
/// bytes it left are no upload of a commit and no chunk of a version, so a collection with no
/// grace deletes them (C15, S7), and Bob, given write again, cannot commit them without
/// uploading again under a commit of his own (S6).
// contract: C15, S7, S6, S4
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bytes_stored_for_an_upload_revoked_in_flight_are_swept() {
    let lab = Lab::start().await;
    let (alice, bob) = (lab.user("alice"), lab.user("bob"));
    let root = lab.create_space(&alice, "team").await;
    set_acl(&lab, &alice, "user:bob", Some("write")).await;

    let bytes = b"revoked in flight".to_vec();
    let release = stall_store(&lab).await;
    let upload = {
        let (url, bob, body) = (lab.url.clone(), bob.clone(), bytes.clone());
        let http = lab.http.clone();
        tokio::spawn(async move {
            let id = ChunkId::of(&body);
            let reply = http
                .put(format!("{url}/v1/spaces/team/commits/r1/chunks/{id}"))
                .bearer_auth(bob)
                .body(body)
                .send()
                .await
                .unwrap();
            (reply.status(), reply.json::<Value>().await.unwrap_or(Value::Null))
        })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!upload.is_finished(), "the store write should be stalled");
    set_acl(&lab, &alice, "user:bob", None).await;
    drop(release);
    let (status, answer) = upload.await.unwrap();
    assert!(
        lab.objects.lock().unwrap().contains_key(&key(&bytes)),
        "{status} {answer}"
    );

    // Given write again, Bob may not commit the bytes the revoked upload left.
    set_acl(&lab, &alice, "user:bob", Some("write")).await;
    let (_, file) = lab
        .rpc(
            &bob,
            &json!({"op": "create", "space": "team", "parent": root, "name": "f", "mode": 420}),
        )
        .await;
    let chunk = json!({"id": ChunkId::of(&bytes).to_string(), "len": bytes.len()});
    let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"],
        "request_id": "r1", "base": null, "guarded": false, "chunks": [chunk]});
    let (committed, reply) = lab.rpc(&bob, &commit).await;
    assert_eq!(
        committed,
        StatusCode::BAD_REQUEST,
        "upload answered {status} {answer}, then commit {reply}"
    );

    let collected_at = now_ms();
    lab.server.sweep(collected_at, &NOTHING_KEPT).await.unwrap();
    lab.server
        .sweep(collected_at + 60_001, &NOTHING_KEPT)
        .await
        .unwrap();
    assert!(
        !lab.objects.lock().unwrap().contains_key(&key(&bytes)),
        "the revoked upload's bytes outlived a sweep with no grace (upload answered {status} {answer})"
    );
}

/// Control: the same stalled upload with write kept is recorded, commits, and its bytes stay.
// contract: C15, C4b
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_upload_with_write_kept_commits_and_stays() {
    let lab = Lab::start().await;
    let (alice, bob) = (lab.user("alice"), lab.user("bob"));
    let root = lab.create_space(&alice, "team").await;
    set_acl(&lab, &alice, "user:bob", Some("write")).await;

    let bytes = b"kept in flight".to_vec();
    let release = stall_store(&lab).await;
    let upload = {
        let (lab_url, bob, body) = (lab.url.clone(), bob.clone(), bytes.clone());
        let http = lab.http.clone();
        tokio::spawn(async move {
            let id = ChunkId::of(&body);
            let reply = http
                .put(format!("{lab_url}/v1/spaces/team/commits/r1/chunks/{id}"))
                .bearer_auth(bob)
                .body(body)
                .send()
                .await
                .unwrap();
            reply.status()
        })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(release);
    assert_eq!(upload.await.unwrap(), StatusCode::OK);

    let (_, file) = lab
        .rpc(
            &bob,
            &json!({"op": "create", "space": "team", "parent": root, "name": "f", "mode": 420}),
        )
        .await;
    let chunk = json!({"id": ChunkId::of(&bytes).to_string(), "len": bytes.len()});
    let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"],
        "request_id": "r1", "base": null, "guarded": false, "chunks": [chunk]});
    let (committed, reply) = lab.rpc(&bob, &commit).await;
    assert_eq!(committed, StatusCode::OK, "{reply}");
    lab.server.sweep(now_ms(), &NOTHING_KEPT).await.unwrap();
    assert!(lab.objects.lock().unwrap().contains_key(&key(&bytes)));
}

/// architecture.md, Retention and collection: "outstanding URLs never race the sweep". A URL
/// granted for the head, which then leaves retention when the next commit lands, names a chunk
/// whose object predates the grace. The sweep used to count from the object's last write and
/// delete it while the URL was seconds old; it now counts from when the chunk left the live set.
/// Grace 120 s is twice the lab's 60 s TTL.
// contract: S4, S7
#[tokio::test]
async fn a_url_granted_just_before_its_version_leaves_retention_keeps_its_chunk() {
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    let root = lab.create_space(&alice, "team").await;
    let (_, file) = lab
        .rpc(
            &alice,
            &json!({"op": "create", "space": "team", "parent": root, "name": "f", "mode": 420}),
        )
        .await;
    let commit = |request_id: &str, bytes: &[u8]| {
        let chunk = json!({"id": ChunkId::of(bytes).to_string(), "len": bytes.len()});
        json!({"op": "commit", "space": "team", "entry": file["entry"]["id"],
            "request_id": request_id, "base": null, "guarded": false, "chunks": [chunk]})
    };
    let (old, new) = (b"old head".to_vec(), b"new head".to_vec());
    lab.upload(&alice, "team", ChunkId::of(&old), old.clone()).await;
    let (_, first) = lab.rpc(&alice, &commit("r1", &old)).await;
    let policy = Policy {
        versions: 1,
        max_age_ms: 0,
        grace_ms: 120_000,
    };
    let collected_at = now_ms() + policy.grace_ms + 1;
    lab.server.sweep(collected_at, &policy).await.unwrap();
    assert!(
        lab.objects.lock().unwrap().contains_key(&key(&old)),
        "control: the head's chunk stays"
    );

    let grant = json!({"op": "grant", "space": "team", "entry": file["entry"]["id"],
        "version": first["version"]["id"], "chunks": [0]});
    let (status, urls) = lab.rpc(&alice, &grant).await;
    assert_eq!(status, StatusCode::OK, "{urls}");
    let granted = std::time::Instant::now();
    lab.upload(&alice, "team", ChunkId::of(&new), new.clone()).await;
    let (status, second) = lab.rpc(&alice, &commit("r2", &new)).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    lab.server
        .sweep(collected_at + policy.grace_ms + 1, &policy)
        .await
        .unwrap();
    let age = granted.elapsed();
    assert!(age < Duration::from_secs(60));
    assert!(
        lab.objects.lock().unwrap().contains_key(&key(&old)),
        "the sweep deleted the chunk of a URL granted {age:?} ago, within its 60 s TTL"
    );
}

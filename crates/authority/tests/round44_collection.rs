//! Red team, round 44: uploads concurrent with a corrected forward clock step.
#![allow(clippy::unwrap_used, reason = "test code")]

mod http;

use std::{sync::Arc, time::Duration};

use axum::http::StatusCode;
use http::{Lab, now_ms};
use herdfs_authority::Policy;
use herdfs_core::ChunkId;
use serde_json::{Value, json};

const GRACE_MS: i64 = 1_000;
const FORWARD_MS: i64 = 60_000;
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

async fn commit(lab: &Lab, token: &str, entry: &Value, request_id: &str, bytes: &[u8]) {
    let chunk = json!({"id": ChunkId::of(bytes).to_string(), "len": bytes.len()});
    let request = json!({"op": "commit", "space": "live", "entry": entry,
        "request_id": request_id, "base": null, "guarded": false, "chunks": [chunk]});
    let (status, reply) = lab.rpc(token, &request).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
}

/// If a sweep starts while the clock is ahead, then the clock is corrected while an upload is
/// in flight, the acknowledged upload and its later committed version must keep their object.
/// A retained file uploaded before the clock step is the control.
// contract: C4b, C14, C15, S7
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "ROUND44: corrected clock during sweep deletes an acknowledged concurrent upload"]
async fn a_corrected_clock_during_a_sweep_does_not_delete_a_concurrent_upload() {
    let mut deleted = 0;
    for trial in 0..3 {
        let lab = Lab::start_with_ttl(Duration::from_millis(1)).await;
        let alice = lab.user("alice");
        let dead_root = lab.create_space(&alice, "dead").await;
        let dead_file = create(&lab, &alice, "dead", &dead_root, "f").await;
        let bytes = format!("round 44 corrected upload {trial}").into_bytes();
        assert_eq!(
            lab.upload(&alice, "dead", ChunkId::of(&bytes), bytes.clone())
                .await
                .0,
            StatusCode::OK
        );
        let chunk = json!({"id": ChunkId::of(&bytes).to_string(), "len": bytes.len()});
        let first = json!({"op": "commit", "space": "dead", "entry": dead_file,
            "request_id": "first", "base": null, "guarded": false, "chunks": [chunk]});
        assert_eq!(lab.rpc(&alice, &first).await.0, StatusCode::OK);
        assert_eq!(
            lab.rpc(&alice, &json!({"op": "delete_space", "space": "dead"}))
                .await
                .0,
            StatusCode::OK
        );

        let live_root = lab.create_space(&alice, "live").await;
        let attack = create(&lab, &alice, "live", &live_root, "attack").await;
        let control = create(&lab, &alice, "live", &live_root, "control").await;
        let control_bytes = format!("round 44 retained control {trial}").into_bytes();
        assert_eq!(
            lab.upload(&alice, "live", ChunkId::of(&control_bytes), control_bytes.clone(),)
                .await
                .0,
            StatusCode::OK
        );
        commit(&lab, &alice, &control, "control", &control_bytes).await;

        let first_sweep = now_ms();
        lab.server.sweep(first_sweep, &POLICY).await.unwrap();
        let store = Arc::clone(&lab.store_writes).acquire_owned().await.unwrap();
        let upload = {
            let (url, token, body, http) = (lab.url.clone(), alice.clone(), bytes.clone(), lab.http.clone());
            tokio::spawn(async move {
                let id = ChunkId::of(&body);
                http.put(format!("{url}/v1/spaces/live/commits/retry/chunks/{id}"))
                    .bearer_auth(token)
                    .body(body)
                    .send()
                    .await
                    .unwrap()
                    .status()
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!upload.is_finished(), "upload was not stalled");

        let sweep = {
            let server = Arc::clone(&lab.server);
            tokio::spawn(async move { server.sweep(first_sweep + FORWARD_MS, &POLICY).await })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!sweep.is_finished(), "sweep did not wait for the upload");
        drop(store);
        assert_eq!(upload.await.unwrap(), StatusCode::OK);
        sweep.await.unwrap().unwrap();

        commit(&lab, &alice, &attack, "retry", &bytes).await;
        let objects = lab.objects.lock().unwrap();
        let lost = !objects.contains_key(&key(&bytes));
        assert!(objects.contains_key(&key(&control_bytes)), "control was deleted");
        eprintln!("trial {trial}: concurrent upload deleted={lost}");
        deleted += usize::from(lost);
    }
    assert_eq!(deleted, 0, "{deleted}/3 acknowledged uploads were deleted");
}

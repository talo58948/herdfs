//! Bucket collection and sweep behavior.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

mod http;

use std::{sync::Arc, time::Duration};

use axum::http::StatusCode;
use http::{Lab, Stored, now_ms};
use herdfs_authority::Policy;
use herdfs_core::ChunkId;
use serde_json::json;

fn key(chunk: &[u8]) -> String {
    format!("herdfs/chunks/{}", ChunkId::of(chunk))
}

#[tokio::test]
async fn a_sweep_deletes_only_chunks_unreferenced_for_the_grace() {
    // contract: C15, S7
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    let root = lab.create_space(&alice, "team").await;
    let bytes: [&[u8]; 3] = [b"first version", b"second version", b"uploaded only"];
    for chunk in bytes {
        assert_eq!(
            lab.upload(&alice, "team", ChunkId::of(chunk), chunk.to_vec())
                .await
                .0,
            StatusCode::OK
        );
    }
    let (_, file) = lab
        .rpc(
            &alice,
            &json!({"op": "create", "space": "team", "parent": root, "name": "f", "mode": 420}),
        )
        .await;
    for (index, chunk) in bytes[..2].iter().enumerate() {
        let chunks = json!([{"id": ChunkId::of(chunk).to_string(), "len": chunk.len()}]);
        let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"],
            "request_id": format!("r{index}"), "base": null, "guarded": false, "chunks": chunks});
        assert_eq!(lab.rpc(&alice, &commit).await.0, StatusCode::OK);
    }
    {
        let mut objects = lab.objects.lock().unwrap();
        for key in [key(b"garbage"), "herdfs/chunks/not-a-chunk".into()] {
            objects.insert(key, Stored { writes: 1, size: 11 });
        }
    }
    let left = || lab.objects.lock().unwrap().keys().cloned().collect::<Vec<_>>();
    let keep_all = Policy {
        versions: 16,
        max_age_ms: i64::MAX / 2,
        grace_ms: 120_000,
    };
    let collected_at = now_ms() + keep_all.grace_ms + 1;
    let swept = lab.server.sweep(collected_at, &keep_all).await.unwrap();
    assert_eq!((swept.pruned, swept.deleted, swept.bytes), (0, 0, 0));

    let head_only = Policy {
        versions: 1,
        max_age_ms: 0,
        grace_ms: keep_all.grace_ms,
    };
    let swept = lab.server.sweep(collected_at + 60_000, &head_only).await.unwrap();
    assert_eq!((swept.pruned, swept.deleted), (1, 0));
    let swept = lab
        .server
        .sweep(collected_at + keep_all.grace_ms + 1, &head_only)
        .await
        .unwrap();
    assert_eq!((swept.pruned, swept.deleted, swept.bytes), (0, 3, 37));
    let mut expected = vec![key(b"second version"), "herdfs/chunks/not-a-chunk".to_owned()];
    expected.sort();
    assert_eq!(left(), expected);
}

#[tokio::test]
async fn a_purged_chunk_goes_on_the_first_sweep_after_the_grace() {
    // contract: S7
    const GRACE_MS: i64 = 120_000;
    const INTERVAL_MS: i64 = 50_000;
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    let root = lab.create_space(&alice, "team").await;
    let (_, file) = lab
        .rpc(
            &alice,
            &json!({"op": "create", "space": "team", "parent": root, "name": "f", "mode": 420}),
        )
        .await;
    let chunks: [&[u8]; 2] = [b"purged", b"head"];
    let policy = Policy {
        versions: 16,
        max_age_ms: i64::MAX / 2,
        grace_ms: GRACE_MS,
    };
    let collected_at = now_ms() + GRACE_MS + 1;
    lab.upload(&alice, "team", ChunkId::of(chunks[0]), chunks[0].to_vec())
        .await;
    lab.server.sweep(collected_at, &policy).await.unwrap();

    // Re-uploading garbage resets its age even when it is committed and purged between sweeps.
    for (index, bytes) in chunks.into_iter().enumerate() {
        let id = ChunkId::of(bytes);
        lab.upload(&alice, "team", id, bytes.to_vec()).await;
        let chunk = json!([{"id": id.to_string(), "len": bytes.len()}]);
        let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"],
            "request_id": format!("r{index}"), "base": null, "guarded": false, "chunks": chunk});
        assert_eq!(lab.rpc(&alice, &commit).await.0, StatusCode::OK);
    }
    lab.rpc(&alice, &json!({"op": "purge", "space": "team", "path": "f"}))
        .await;

    for elapsed in [INTERVAL_MS, 2 * INTERVAL_MS] {
        let swept = lab.server.sweep(collected_at + elapsed, &policy).await.unwrap();
        assert_eq!(swept.deleted, 0, "deleted after {elapsed} ms");
        assert!(lab.objects.lock().unwrap().contains_key(&key(chunks[0])));
    }
    let swept = lab
        .server
        .sweep(collected_at + GRACE_MS + INTERVAL_MS, &policy)
        .await
        .unwrap();
    assert_eq!(swept.deleted, 1);
    assert!(!lab.objects.lock().unwrap().contains_key(&key(chunks[0])));
}

#[tokio::test]
async fn collection_runs_periodically() {
    // contract: C14, C15, S7
    let lab = Lab::start_with_ttl(Duration::from_millis(1)).await;
    let alice = lab.user("alice");
    let root = lab.create_space(&alice, "team").await;
    let (_, file) = lab
        .rpc(
            &alice,
            &json!({"op": "create", "space": "team", "parent": root, "name": "f", "mode": 420}),
        )
        .await;
    let chunks: [&[u8]; 2] = [b"pruned", b"head"];
    for (index, bytes) in chunks.into_iter().enumerate() {
        let id = ChunkId::of(bytes);
        lab.upload(&alice, "team", id, bytes.to_vec()).await;
        let chunk = json!([{"id": id.to_string(), "len": bytes.len()}]);
        let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"],
            "request_id": format!("r{index}"), "base": null, "guarded": false, "chunks": chunk});
        assert_eq!(lab.rpc(&alice, &commit).await.0, StatusCode::OK);
    }
    let pruned = key(chunks[0]);
    let policy = Policy {
        versions: 1,
        max_age_ms: 0,
        grace_ms: 0,
    };
    let collector = tokio::spawn(Arc::clone(&lab.server).collect_every(Duration::from_millis(10), policy));
    let gone = tokio::time::timeout(Duration::from_secs(1), async {
        while lab.objects.lock().unwrap().contains_key(&pruned) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    collector.abort();
    assert!(gone.is_ok(), "periodic collection left {pruned}");
}

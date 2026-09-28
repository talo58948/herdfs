//! The HTTP routes: chunk uploads, presigned grants and error statuses.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod http;

use axum::http::StatusCode;
use http::{Lab, PUBLIC_S3, Stored, now_ms};
use herdfs_authority::Policy;
use herdfs_core::{ChunkId, chunk::MAX_CHUNK};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn only_a_caller_who_may_upload_causes_a_store_write() {
    // contract: S6, S1
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    lab.rpc(&alice, &json!({"op": "create_space", "space": "team"}))
        .await;
    let bytes = b"chunk bytes".to_vec();
    let id = ChunkId::of(&bytes);
    let refused = [
        (
            alice.as_str(),
            "team",
            b"other bytes".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (&alice, "team", vec![0; MAX_CHUNK + 1], StatusCode::BAD_REQUEST),
        ("bad", "team", bytes.clone(), StatusCode::UNAUTHORIZED),
        (&lab.user("bob"), "team", bytes.clone(), StatusCode::FORBIDDEN),
        (&alice, "nowhere", bytes.clone(), StatusCode::NOT_FOUND),
    ];
    for (token, space, body, status) in refused {
        assert_eq!(lab.upload(token, space, id, body).await.0, status);
    }
    assert!(lab.objects.lock().unwrap().is_empty());
    // Every accepted upload is written, even of bytes already stored.
    for _ in 0..2 {
        let reply = lab.upload(&alice, "team", id, bytes.clone()).await;
        assert_eq!(reply, (StatusCode::OK, json!({"type": "done"})));
    }
    assert_eq!(
        lab.objects
            .lock()
            .unwrap()
            .get(&format!("herdfs/chunks/{id}"))
            .map(|stored| stored.writes),
        Some(2)
    );
}

#[tokio::test]
async fn an_uploaded_chunk_is_committable_and_granted_on_the_public_endpoint() {
    // contract: S6, S2, S1
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    lab.rpc(&alice, &json!({"op": "create_space", "space": "team"}))
        .await;
    let bytes = b"chunk bytes".to_vec();
    let id = ChunkId::of(&bytes);
    let key = format!("herdfs/chunks/{id}");
    assert_eq!(
        lab.upload(&alice, "team", id, bytes.clone()).await.0,
        StatusCode::OK
    );

    let (_, file) = lab
        .rpc(
            &alice,
            &json!({"op": "create", "space": "team", "path": "f", "mode": 420}),
        )
        .await;
    let chunks = json!([{"id": id.to_string(), "len": bytes.len()}]);
    let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"], "request_id": "r1",
        "base": null, "guarded": false, "chunks": chunks});
    let before_ms = now_ms();
    let (status, version) = lab.rpc(&alice, &commit).await;
    assert_eq!(status, StatusCode::OK, "{version}");
    // The server stamps versions with the wall clock.
    let created_ms = version["version"]["created_ms"].as_i64().unwrap();
    assert!(
        (before_ms..=now_ms()).contains(&created_ms),
        "created at {created_ms}"
    );

    let grant = json!({"op": "grant", "space": "team", "entry": file["entry"]["id"],
        "version": version["version"]["id"], "chunks": [0]});
    let (status, grants) = lab.rpc(&alice, &grant).await;
    assert_eq!(status, StatusCode::OK, "{grants}");
    let url = grants["urls"][0].as_str().unwrap();
    // contract: S2, S4 — presigned for the public endpoint, valid for the 60 s grant TTL.
    assert!(url.starts_with(&format!("{PUBLIC_S3}/{key}?")), "{url}");
    assert!(url.contains("X-Amz-Expires=60"), "{url}");
    assert_eq!(grants["ttl_ms"], json!(60_000));
}

#[tokio::test]
async fn errors_carry_their_code_and_status() {
    let lab = Lab::start().await;
    let (alice, bob) = (lab.user("alice"), lab.user("bob"));
    lab.rpc(&alice, &json!({"op": "create_space", "space": "team"}))
        .await;
    let create = json!({"op": "create", "space": "team", "path": "f", "mode": 420});
    let stat = |space: &str, path: &str| json!({"op": "stat", "space": space, "path": path});
    let cases = [
        (&alice, create.clone(), StatusCode::OK, Value::Null),
        (&alice, create, StatusCode::CONFLICT, json!("exists")),
        (
            &alice,
            stat("team", "missing"),
            StatusCode::NOT_FOUND,
            json!("not_found"),
        ),
        (
            &alice,
            stat("nowhere", ""),
            StatusCode::NOT_FOUND,
            json!("not_found"),
        ),
        (&bob, stat("team", "f"), StatusCode::FORBIDDEN, json!("denied")),
        (
            &alice,
            json!({"op": "remove", "space": "team", "path": ""}),
            StatusCode::BAD_REQUEST,
            json!("invalid"),
        ),
        (
            &alice,
            json!({"op": "stat", "space": "team", "path": "../x"}),
            StatusCode::BAD_REQUEST,
            json!("invalid"),
        ),
    ];
    for (who, request, status, code) in cases {
        let (got, reply) = lab.rpc(who, &request).await;
        assert_eq!(got, status, "{request} answered {reply}");
        if !code.is_null() {
            assert_eq!(reply["code"], code, "{request}");
        }
    }
    let health = lab
        .http
        .get(format!("{}/v1/health", lab.url))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_sweep_deletes_only_unreferenced_chunks_older_than_the_grace() {
    // contract: C15, S7
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    lab.rpc(&alice, &json!({"op": "create_space", "space": "team"}))
        .await;
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
            &json!({"op": "create", "space": "team", "path": "f", "mode": 420}),
        )
        .await;
    for (n, chunk) in bytes[..2].iter().enumerate() {
        let chunks = json!([{"id": ChunkId::of(chunk).to_string(), "len": chunk.len()}]);
        let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"],
            "request_id": format!("r{n}"), "base": null, "guarded": false, "chunks": chunks});
        assert_eq!(lab.rpc(&alice, &commit).await.0, StatusCode::OK);
    }
    let key = |chunk: &[u8]| format!("herdfs/chunks/{}", ChunkId::of(chunk));
    let hour_ago = now_ms() - 3_600_000;
    {
        let mut objects = lab.objects.lock().unwrap();
        for chunk in bytes {
            objects.get_mut(&key(chunk)).unwrap().modified_ms = hour_ago;
        }
        let garbage = [
            (key(b"old garbage"), hour_ago),
            (key(b"new garbage"), now_ms()),
            ("herdfs/chunks/not-a-chunk".into(), hour_ago),
        ];
        for (key, modified_ms) in garbage {
            objects.insert(
                key,
                Stored {
                    writes: 1,
                    size: 11,
                    modified_ms,
                },
            );
        }
    }
    let left = || lab.objects.lock().unwrap().keys().cloned().collect::<Vec<_>>();

    // Retained versions and fresh uploads stay; so do young objects and keys that name no chunk.
    let keep_all = Policy {
        versions: 16,
        max_age_ms: i64::MAX / 2,
        grace_ms: 60_000,
    };
    let swept = lab.server.sweep(&keep_all).await.unwrap();
    assert_eq!((swept.pruned, swept.deleted, swept.bytes), (0, 1, 11));
    assert!(!left().contains(&key(b"old garbage")));

    // With no grace, uploads expire and the pruned first version's chunk goes too.
    let head_only = Policy {
        versions: 1,
        max_age_ms: 0,
        grace_ms: 0,
    };
    let swept = lab.server.sweep(&head_only).await.unwrap();
    assert_eq!((swept.pruned, swept.deleted), (1, 3));
    let mut expected = vec![key(b"second version"), "herdfs/chunks/not-a-chunk".to_owned()];
    expected.sort();
    assert_eq!(left(), expected);
}

#[tokio::test]
async fn collection_runs_periodically() {
    // contract: C14, C15, S7
    let lab = Lab::start().await;
    let alice = lab.user("alice");
    lab.rpc(&alice, &json!({"op": "create_space", "space": "team"}))
        .await;
    let (_, file) = lab
        .rpc(
            &alice,
            &json!({"op": "create", "space": "team", "path": "f", "mode": 420}),
        )
        .await;
    let chunks: [&[u8]; 2] = [b"pruned", b"head"];
    for (index, bytes) in chunks.into_iter().enumerate() {
        let id = ChunkId::of(bytes);
        assert_eq!(
            lab.upload(&alice, "team", id, bytes.to_vec()).await.0,
            StatusCode::OK
        );
        let chunk = json!([{"id": id.to_string(), "len": bytes.len()}]);
        let commit = json!({"op": "commit", "space": "team", "entry": file["entry"]["id"],
            "request_id": format!("r{index}"), "base": null, "guarded": false, "chunks": chunk});
        assert_eq!(lab.rpc(&alice, &commit).await.0, StatusCode::OK);
    }
    let pruned = format!("herdfs/chunks/{}", ChunkId::of(chunks[0]));
    lab.objects.lock().unwrap().get_mut(&pruned).unwrap().modified_ms = 0;
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

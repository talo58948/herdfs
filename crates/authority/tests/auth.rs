//! Bearer tokens: signature, issuer, audience, expiry and key ids.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod http;

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use http::{KEY, Lab, NONE_HEADER, ROGUE_KEY, now};
use serde_json::{Value, json};

#[tokio::test]
async fn tokens_need_a_known_key_issuer_audience_and_future_expiry() {
    // contract: S5
    let lab = Lab::start().await;
    let (status, me) = lab.rpc(&lab.user("alice"), &json!({"op": "whoami"})).await;
    assert_eq!(
        (status, me),
        (
            StatusCode::OK,
            json!({"type": "me", "principal": "user:alice", "groups": ["eng"]})
        )
    );

    let with = |field: &str, value: Value| {
        let mut claims = lab.claims("alice");
        claims[field] = value;
        Lab::token(&claims, KEY, "k1")
    };
    let payload = lab.user("alice").split('.').nth(1).unwrap().to_owned();
    let bad = [
        with("exp", json!(now() - 3600)),
        with("aud", json!("elsewhere")),
        with("iss", json!("http://impostor")),
        Lab::token(&lab.claims("alice"), ROGUE_KEY, "k1"),
        Lab::token(&lab.claims("alice"), KEY, "k9"),
        format!("{NONE_HEADER}.{payload}."),
        "not a token".into(),
    ];
    for token in bad {
        let (status, error) = lab.rpc(&token, &json!({"op": "whoami"})).await;
        assert_eq!(
            (status, &error["code"]),
            (StatusCode::UNAUTHORIZED, &json!("unauthenticated")),
            "{token}"
        );
    }
    let reply = lab
        .http
        .post(format!("{}/v1/rpc", lab.url))
        .json(&json!({"op": "whoami"}))
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_key_ids_refetch_the_keys_at_most_once_per_30_seconds() {
    // contract: S5
    let lab = Lab::start().await;
    for _ in 0..3 {
        let (status, _) = lab
            .rpc(
                &Lab::token(&lab.claims("alice"), KEY, "k9"),
                &json!({"op": "whoami"}),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        lab.rpc(&lab.user("alice"), &json!({"op": "whoami"})).await.0,
        StatusCode::OK
    );
    assert_eq!(lab.jwks_fetches.load(Ordering::SeqCst), 1);
}

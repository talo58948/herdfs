//! A running authority server, with an in-process OIDC issuer and a fake S3
//! endpoint that records which objects the authority writes.
#![allow(dead_code, reason = "each test binary uses a different subset")]

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query},
    http::StatusCode,
    routing::{get, put},
};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use herdfs_authority::{Authority, server::Server};
use herdfs_core::ChunkId;
use reqwest::Url;
use rusty_s3::{Bucket, Credentials, UrlStyle};
use serde_json::{Value, json};
use tempfile::TempDir;

// Test-only Ed25519 keys, the same as the lab's.
pub const KEY: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIBw/bJ0yA+lDH10LbwjU8FNz8W/WoCxXdiIO2ouDIUo+
-----END PRIVATE KEY-----";
pub const KEY_X: &str = "1YXvpZn5rWDm4IkMDok-qEpwMr1Omjfvq7Gb0D3gI5I";
pub const ROGUE_KEY: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIBP07PlepLfvlKZqgy6JhKsWVMTh/6B+w2gP1/Cem1JL
-----END PRIVATE KEY-----";
/// `{"alg":"none","typ":"JWT"}`, base64url.
pub const NONE_HEADER: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0";
pub const PUBLIC_S3: &str = "http://s3.lab:9000";

pub struct Lab {
    pub server: Arc<Server>,
    pub url: String,
    pub issuer: String,
    pub jwks_fetches: Arc<AtomicUsize>,
    /// The fake bucket's objects, by key.
    pub objects: Store,
    pub http: reqwest::Client,
    _dir: TempDir,
}

/// An object in the fake bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    pub writes: usize,
    pub size: usize,
    pub modified_ms: i64,
}

pub type Store = Arc<Mutex<BTreeMap<String, Stored>>>;

/// A bucket that takes PUT and DELETE, and lists with ListObjectsV2 two keys a page.
fn fake_s3(objects: Store) -> Router {
    let (puts, deletes) = (Arc::clone(&objects), Arc::clone(&objects));
    let put_object = move |Path(key): Path<String>, body: Bytes| async move {
        let mut objects = puts.lock().unwrap();
        let writes = objects.get(&key).map_or(0, |stored| stored.writes);
        objects.insert(
            key,
            Stored {
                writes: writes + 1,
                size: body.len(),
                modified_ms: now_ms(),
            },
        );
    };
    let delete_object = move |Path(key): Path<String>| async move {
        deletes.lock().unwrap().remove(&key);
    };
    let list = move |Path(bucket): Path<String>, Query(query): Query<BTreeMap<String, String>>| async move {
        let bucket = bucket.trim_end_matches('/');
        let prefix = format!("{bucket}/{}", query.get("prefix").map_or("", String::as_str));
        let skip: usize = query
            .get("continuation-token")
            .map_or(0, |token| token.parse().unwrap());
        let objects = objects.lock().unwrap();
        let matching: Vec<_> = objects
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .collect();
        let contents: String = matching.iter().skip(skip).take(2).map(|(key, stored)| {
            let (key, at) = (&key[bucket.len() + 1..], jiff::Timestamp::from_millisecond(stored.modified_ms).unwrap());
            format!("<Contents><Key>{key}</Key><LastModified>{at}</LastModified><ETag>\"x\"</ETag><Size>{}</Size></Contents>", stored.size)
        }).collect();
        let next = if skip + 2 < matching.len() {
            format!("<NextContinuationToken>{}</NextContinuationToken>", skip + 2)
        } else {
            String::new()
        };
        format!(
            r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">{contents}{next}</ListBucketResult>"#
        )
    };
    Router::new().route("/{*key}", put(put_object).delete(delete_object).get(list))
}

/// Serves the router built for its own base URL.
async fn serve(router: impl FnOnce(&str) -> Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = router(&url);
    tokio::spawn(async move { axum::serve(listener, router).await });
    url
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

pub fn now_ms() -> i64 {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

impl Lab {
    pub async fn start() -> Self {
        let jwks_fetches = Arc::new(AtomicUsize::new(0));
        let fetches = Arc::clone(&jwks_fetches);
        let issuer = serve(|url| {
            let discovery = json!({"issuer": url, "jwks_uri": format!("{url}/jwks")});
            let jwk = json!({"kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "kid": "k1", "x": KEY_X});
            let jwks = move || async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                Json(json!({"keys": [jwk]}))
            };
            Router::new()
                .route(
                    "/.well-known/openid-configuration",
                    get(|| async { Json(discovery) }),
                )
                .route("/jwks", get(jwks))
        })
        .await;
        let objects = Store::default();
        let s3 = serve(|_| fake_s3(Arc::clone(&objects))).await;
        let dir = tempfile::tempdir().unwrap();
        let authority = Authority::open(&dir.path().join("authority.db")).unwrap();
        let bucket = |url: &str| {
            Bucket::new(Url::parse(url).unwrap(), UrlStyle::Path, "herdfs", "us-east-1").unwrap()
        };
        let credentials = Credentials::new("key", "secret");
        let server = Server::new(
            authority,
            bucket(&s3),
            bucket(PUBLIC_S3),
            credentials,
            issuer.clone(),
            "herdfs".into(),
            std::time::Duration::from_secs(60),
        );
        let server = Arc::new(server);
        let url = serve(|_| server.router()).await;
        Self {
            server,
            url,
            issuer,
            jwks_fetches,
            objects,
            http: reqwest::Client::new(),
            _dir: dir,
        }
    }

    pub fn claims(&self, sub: &str) -> Value {
        json!({"iss": self.issuer, "aud": "herdfs", "sub": sub, "exp": now() + 600, "groups": ["eng"]})
    }

    pub fn token(claims: &Value, key: &str, kid: &str) -> String {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(kid.into());
        jsonwebtoken::encode(
            &header,
            claims,
            &EncodingKey::from_ed_pem(key.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    pub fn user(&self, sub: &str) -> String {
        Self::token(&self.claims(sub), KEY, "k1")
    }

    pub async fn rpc(&self, token: &str, body: &Value) -> (StatusCode, Value) {
        let call = self
            .http
            .post(format!("{}/v1/rpc", self.url))
            .bearer_auth(token)
            .json(body);
        let reply = call.send().await.unwrap();
        (reply.status(), reply.json().await.unwrap())
    }

    pub async fn upload(&self, token: &str, space: &str, id: ChunkId, body: Vec<u8>) -> (StatusCode, Value) {
        // One commit per upload, so each upload ages on its own.
        let url = format!("{}/v1/spaces/{space}/commits/{id}/chunks/{id}", self.url);
        let reply = self
            .http
            .put(url)
            .bearer_auth(token)
            .body(body)
            .send()
            .await
            .unwrap();
        let status = reply.status();
        (status, reply.json().await.unwrap_or(Value::Null))
    }
}

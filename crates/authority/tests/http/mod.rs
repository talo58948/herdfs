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
    extract::Path,
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
    pub url: String,
    pub issuer: String,
    pub jwks_fetches: Arc<AtomicUsize>,
    /// Each object key the authority wrote, with how often.
    pub objects: Arc<Mutex<BTreeMap<String, usize>>>,
    pub http: reqwest::Client,
    _dir: TempDir,
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
        let objects = Arc::new(Mutex::new(BTreeMap::new()));
        let stored = Arc::clone(&objects);
        let write = move |Path(key): Path<String>| async move {
            *stored.lock().unwrap().entry(key).or_insert(0) += 1;
        };
        let s3 = serve(|_| Router::new().route("/{*key}", put(write))).await;
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
        );
        let url = serve(|_| server.router()).await;
        Self {
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
        let url = format!("{}/v1/spaces/{space}/chunks/{id}", self.url);
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

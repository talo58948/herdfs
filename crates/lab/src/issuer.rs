//! A test OIDC issuer: a discovery document, a JWKS, and tokens good and bad.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::extract::{Form, Query, State};
use axum::http::StatusCode;
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::runtime::{self, Runtime};

/// The audience the authority accepts.
pub const AUDIENCE: &str = "herdfs";
const KEY_ID: &str = "lab";
// Test-only Ed25519 keys, made with `openssl genpkey -algorithm ed25519`.
const KEY: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIBw/bJ0yA+lDH10LbwjU8FNz8W/WoCxXdiIO2ouDIUo+
-----END PRIVATE KEY-----";
/// The public half of `KEY`, as a JWK's base64url `x`.
const KEY_X: &str = "1YXvpZn5rWDm4IkMDok-qEpwMr1Omjfvq7Gb0D3gI5I";
const ROGUE_KEY: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIBP07PlepLfvlKZqgy6JhKsWVMTh/6B+w2gP1/Cem1JL
-----END PRIVATE KEY-----";
/// `{"alg":"none","typ":"JWT"}`, base64url.
const NONE_HEADER: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0";
const LOGIN_SUB: &str = "login";
/// Long enough to outlast any test run.
const TOKEN_TTL: Duration = Duration::from_secs(24 * 3600);
const LOGIN_TTL: Duration = Duration::from_secs(35);
const REFRESH_TTL: Duration = Duration::from_secs(120);

#[derive(Clone)]
struct AppState {
    url: String,
    key: Arc<EncodingKey>,
    codes: Arc<Mutex<HashMap<String, Code>>>,
    refresh_tokens: Arc<Mutex<HashMap<String, String>>>,
    refresh_count: Arc<AtomicU64>,
}

struct Code {
    challenge: String,
    client_id: String,
    redirect_uri: String,
    sub: String,
}

/// A token the authority must reject (contract S5). Each is valid except for its one defect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadToken {
    /// Expired an hour ago.
    Expired,
    /// Issued for another audience.
    WrongAudience,
    /// Signed by a key that is not in the JWKS, under the id of the key that is.
    UnknownKey,
    /// `alg: none`, without a signature.
    AlgNone,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    groups: Vec<&'a str>,
    iat: u64,
    exp: u64,
}

/// Serves discovery and a JWKS on 127.0.0.1 and mints tokens, until dropped.
pub struct Issuer {
    url: String,
    key: Arc<EncodingKey>,
    rogue: EncodingKey,
    refresh_count: Arc<AtomicU64>,
    /// Serves discovery and the JWKS until the issuer is dropped.
    _runtime: Runtime,
}

impl Issuer {
    pub fn start() -> Result<Self> {
        let runtime = runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0"))?;
        let url = format!("http://{}", listener.local_addr()?);
        let key = Arc::new(EncodingKey::from_ed_pem(KEY.as_bytes())?);
        let refresh_count = Arc::new(AtomicU64::new(0));
        let state = AppState {
            url: url.clone(),
            key: key.clone(),
            codes: Arc::default(),
            refresh_tokens: Arc::default(),
            refresh_count: refresh_count.clone(),
        };
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .with_state(state);
        runtime.spawn(async { axum::serve(listener, app).await });
        Ok(Self {
            url,
            key,
            rogue: EncodingKey::from_ed_pem(ROGUE_KEY.as_bytes())?,
            refresh_count,
            _runtime: runtime,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn token(&self, sub: &str, groups: &[impl AsRef<str>]) -> String {
        sign(&self.claims(sub, groups), &self.key)
    }

    pub fn refresh_count(&self) -> u64 {
        self.refresh_count.load(Ordering::Relaxed)
    }

    pub(crate) fn bad_token(&self, sub: &str, kind: BadToken) -> String {
        let claims = self.claims(sub, &[] as &[&str]);
        match kind {
            BadToken::Expired => {
                let (iat, exp) = (claims.iat - 7200, claims.iat - 3600);
                sign(&Claims { iat, exp, ..claims }, &self.key)
            }
            BadToken::WrongAudience => sign(
                &Claims {
                    aud: "elsewhere",
                    ..claims
                },
                &self.key,
            ),
            BadToken::UnknownKey => sign(&claims, &self.rogue),
            BadToken::AlgNone => {
                let signed = sign(&claims, &self.key);
                let payload = signed.split('.').nth(1).unwrap_or_default();
                format!("{NONE_HEADER}.{payload}.")
            }
        }
    }

    fn claims<'a>(&'a self, sub: &'a str, groups: &'a [impl AsRef<str>]) -> Claims<'a> {
        claims(&self.url, sub, groups, TOKEN_TTL)
    }
}

fn claims<'a>(issuer: &'a str, sub: &'a str, groups: &'a [impl AsRef<str>], ttl: Duration) -> Claims<'a> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Claims {
        iss: issuer,
        sub,
        aud: AUDIENCE,
        groups: groups.iter().map(AsRef::as_ref).collect(),
        iat: now,
        exp: now + ttl.as_secs(),
    }
}

#[expect(
    clippy::expect_used,
    reason = "the embedded keys are valid and signing Ed25519 cannot fail"
)]
fn sign(claims: &Claims, key: &EncodingKey) -> String {
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(KEY_ID.to_owned());
    jsonwebtoken::encode(&header, claims, key).expect("the embedded test keys sign any claims")
}

async fn discovery(State(state): State<AppState>) -> Json<Value> {
    let url = state.url;
    Json(json!({
        "issuer": url,
        "jwks_uri": format!("{url}/jwks"),
        "authorization_endpoint": format!("{url}/authorize"),
        "token_endpoint": format!("{url}/token"),
        "id_token_signing_alg_values_supported": ["EdDSA"],
    }))
}

async fn jwks() -> Json<Value> {
    Json(json!({"keys": [{
        "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig", "kid": KEY_ID, "x": KEY_X,
    }]}))
}

async fn authorize(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Redirect, StatusCode> {
    let get = |name| query.get(name).cloned().ok_or(StatusCode::BAD_REQUEST);
    if get("response_type")? != "code" || get("code_challenge_method")? != "S256" {
        return Err(StatusCode::BAD_REQUEST);
    }
    let scope = get("scope")?;
    if !["openid", "offline_access"]
        .into_iter()
        .all(|wanted| scope.split_whitespace().any(|scope| scope == wanted))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let redirect_uri = get("redirect_uri")?;
    let state_parameter = get("state")?;
    let code = format!("{:032x}", rand::random::<u128>());
    let grant = Code {
        challenge: get("code_challenge")?,
        client_id: get("client_id")?,
        redirect_uri: redirect_uri.clone(),
        sub: LOGIN_SUB.into(),
    };
    lock(&state.codes).insert(code.clone(), grant);
    let mut redirect = reqwest::Url::parse(&redirect_uri).map_err(|_| StatusCode::BAD_REQUEST)?;
    redirect
        .query_pairs_mut()
        .extend_pairs([("code", code.as_str()), ("state", state_parameter.as_str())]);
    Ok(Redirect::to(redirect.as_str()))
}

async fn token(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => authorization_code(&state, &form),
        Some("refresh_token") => refresh_token(&state, &form).await,
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

fn authorization_code(state: &AppState, form: &HashMap<String, String>) -> Result<Json<Value>, StatusCode> {
    let code = form.get("code").ok_or(StatusCode::BAD_REQUEST)?;
    let grant = lock(&state.codes).remove(code).ok_or(StatusCode::BAD_REQUEST)?;
    let verifier = form.get("code_verifier").ok_or(StatusCode::BAD_REQUEST)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    if challenge != grant.challenge
        || form.get("client_id") != Some(&grant.client_id)
        || form.get("redirect_uri") != Some(&grant.redirect_uri)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let refresh = format!("{:032x}", rand::random::<u128>());
    lock(&state.refresh_tokens).insert(refresh.clone(), grant.sub.clone());
    Ok(tokens(state, &grant.sub, &refresh, LOGIN_TTL))
}

async fn refresh_token(state: &AppState, form: &HashMap<String, String>) -> Result<Json<Value>, StatusCode> {
    let refresh = form.get("refresh_token").ok_or(StatusCode::BAD_REQUEST)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let sub = lock(&state.refresh_tokens)
        .remove(refresh)
        .ok_or(StatusCode::BAD_REQUEST)?;
    let next = format!("{:032x}", rand::random::<u128>());
    lock(&state.refresh_tokens).insert(next.clone(), sub.clone());
    state.refresh_count.fetch_add(1, Ordering::Relaxed);
    Ok(tokens(state, &sub, &next, REFRESH_TTL))
}

fn tokens(state: &AppState, sub: &str, refresh: &str, ttl: Duration) -> Json<Value> {
    let access = sign(&claims(&state.url, sub, &[] as &[&str], ttl), &state.key);
    Json(json!({
        "access_token": access,
        "refresh_token": refresh,
        "token_type": "Bearer",
        "expires_in": ttl.as_secs(),
    }))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

//! A test OIDC issuer: a discovery document, a JWKS, and tokens good and bad.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::runtime::{self, Runtime};

/// The audience the authority accepts.
pub(crate) const AUDIENCE: &str = "herdfs";
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
/// Long enough to outlast any test run.
const TOKEN_TTL: Duration = Duration::from_secs(24 * 3600);

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

pub(crate) struct Issuer {
    url: String,
    key: EncodingKey,
    rogue: EncodingKey,
    /// Serves discovery and the JWKS until the issuer is dropped.
    _runtime: Runtime,
}

impl Issuer {
    pub(crate) fn start() -> Result<Self> {
        let runtime = runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0"))?;
        let url = format!("http://{}", listener.local_addr()?);
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .with_state(url.clone());
        runtime.spawn(async { axum::serve(listener, app).await });
        Ok(Self {
            url,
            key: EncodingKey::from_ed_pem(KEY.as_bytes())?,
            rogue: EncodingKey::from_ed_pem(ROGUE_KEY.as_bytes())?,
            _runtime: runtime,
        })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) fn token(&self, sub: &str, groups: &[impl AsRef<str>]) -> String {
        sign(&self.claims(sub, groups), &self.key)
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Claims {
            iss: &self.url,
            sub,
            aud: AUDIENCE,
            groups: groups.iter().map(AsRef::as_ref).collect(),
            iat: now,
            exp: now + TOKEN_TTL.as_secs(),
        }
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

async fn discovery(State(url): State<String>) -> Json<Value> {
    Json(json!({
        "issuer": url,
        "jwks_uri": format!("{url}/jwks"),
        "id_token_signing_alg_values_supported": ["EdDSA"],
    }))
}

async fn jwks() -> Json<Value> {
    Json(json!({"keys": [{
        "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig", "kid": KEY_ID, "x": KEY_X,
    }]}))
}

//! Bearer tokens, checked against the keys the OIDC issuer publishes.

use std::time::{Duration, Instant};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use herdfs_core::proto::{self, ErrorCode};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::Principal;

/// An unknown key id refetches the issuer's keys at most this often, so
/// forged key ids cannot flood the issuer.
const REFETCH_EVERY: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct Verifier {
    issuer: String,
    audience: String,
    http: reqwest::Client,
    keys: Mutex<Keys>,
}

#[derive(Default)]
struct Keys {
    set: JwkSet,
    fetched: Option<Instant>,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    groups: Vec<String>,
}

#[derive(Deserialize)]
struct Discovery {
    jwks_uri: String,
}

impl Verifier {
    pub(super) fn new(issuer: String, audience: String) -> Self {
        Self {
            issuer,
            audience,
            http: reqwest::Client::new(),
            keys: Mutex::default(),
        }
    }

    /// The caller named by the request's bearer token.
    pub(super) async fn principal(&self, headers: &HeaderMap) -> Result<Principal, proto::Error> {
        let token = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| unauthenticated("missing bearer token"))?;
        // Fails for `alg: none`, which jsonwebtoken does not know.
        let header = decode_header(token).map_err(|error| unauthenticated(error.to_string()))?;
        let kid = header.kid.ok_or_else(|| unauthenticated("token has no key id"))?;
        let key = self.key(&kid).await?;
        // Pinning the key's family stops a token from picking a weaker algorithm.
        let mut validation = Validation::new_for_family(key.family());
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|error| unauthenticated(error.to_string()))?
            .claims;
        Ok(Principal {
            user: format!("user:{}", claims.sub),
            groups: claims.groups,
        })
    }

    async fn key(&self, kid: &str) -> Result<DecodingKey, proto::Error> {
        let mut keys = self.keys.lock().await;
        let may_refetch = keys.fetched.is_none_or(|at| at.elapsed() >= REFETCH_EVERY);
        if keys.set.find(kid).is_none() && may_refetch {
            keys.set = self.fetch().await.map_err(|error| {
                proto::Error::new(
                    ErrorCode::Unavailable,
                    format!("fetching the issuer's keys: {error}"),
                )
            })?;
            keys.fetched = Some(Instant::now());
        }
        let jwk = keys
            .set
            .find(kid)
            .ok_or_else(|| unauthenticated(format!("unknown key id {kid:?}")))?;
        DecodingKey::from_jwk(jwk).map_err(|error| unauthenticated(error.to_string()))
    }

    async fn fetch(&self) -> reqwest::Result<JwkSet> {
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        );
        let discovery: Discovery = self.get(url).await?.json().await?;
        self.get(discovery.jwks_uri).await?.json().await
    }

    async fn get(&self, url: String) -> reqwest::Result<reqwest::Response> {
        self.http
            .get(url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await?
            .error_for_status()
    }
}

fn unauthenticated(reason: impl Into<String>) -> proto::Error {
    proto::Error::new(ErrorCode::Unauthenticated, reason)
}

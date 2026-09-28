//! Bearer tokens, checked against the keys the OIDC issuer publishes.

use std::time::{Duration, Instant};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use herdfs_core::proto::{self, ErrorCode};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::Principal;

/// The issuer's keys are fetched again this often, so a key it withdraws
/// stops working and a new one starts within this time.
const REFETCH_EVERY: Duration = Duration::from_secs(30);
/// While the issuer is unreachable, its last keys stay trusted this long.
const TRUST_AT_MOST: Duration = Duration::from_secs(5 * 60);
/// Clock skew allowed past a token's `exp`.
const EXP_LEEWAY_S: u64 = 5;
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
    /// The last fetch, successful or not.
    tried: Option<Instant>,
    /// The last successful fetch.
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
        validation.leeway = EXP_LEEWAY_S;
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
        let due = keys.tried.is_none_or(|at| at.elapsed() >= REFETCH_EVERY);
        if keys.fetched.is_none() || due {
            keys.tried = Some(Instant::now());
            match self.fetch().await {
                Ok(set) => (keys.set, keys.fetched) = (set, keys.tried),
                Err(error) => tracing::warn!(%error, "fetching the issuer's keys"),
            }
        }
        if keys.fetched.is_none_or(|at| at.elapsed() >= TRUST_AT_MOST) {
            let message = "the issuer's keys could not be fetched";
            return Err(proto::Error::new(ErrorCode::Unavailable, message));
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

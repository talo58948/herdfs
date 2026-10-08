//! Bearer tokens, checked against the keys the OIDC issuer publishes.

use std::time::{Duration, Instant};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use herdfs_core::proto::{self, ErrorCode};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::Principal;
use super::unauthenticated;

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
    refreshing: Mutex<()>,
}

pub(super) struct Authenticated {
    pub(super) principal: Principal,
    pub(super) subject: String,
    pub(super) token_id: Option<String>,
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
    jti: Option<String>,
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
            refreshing: Mutex::new(()),
        }
    }

    pub(super) fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The caller named by the request's bearer token.
    pub(super) async fn authenticate(&self, headers: &HeaderMap) -> Result<Authenticated, proto::Error> {
        let token = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| unauthenticated("missing bearer token"))?;
        // Fails for `alg: none`, which jsonwebtoken does not know.
        let header = decode_header(token).map_err(|error| unauthenticated(error.to_string()))?;
        let kid = header.kid.ok_or_else(|| unauthenticated("token has no key id"))?;
        if let Ok(authenticated) = self.validate(token, &kid).await {
            if let Ok(_refreshing) = self.refreshing.try_lock() {
                self.refresh().await;
            }
            return Ok(authenticated);
        }
        let _refreshing = self.refreshing.lock().await;
        self.refresh().await;
        self.validate(token, &kid).await
    }

    async fn validate(&self, token: &str, kid: &str) -> Result<Authenticated, proto::Error> {
        let key = self.key(kid).await?;
        // Pinning the key's family stops a token from picking a weaker algorithm.
        let mut validation = Validation::new_for_family(key.family());
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = EXP_LEEWAY_S;
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|error| unauthenticated(error.to_string()))?
            .claims;
        Ok(Authenticated {
            principal: Principal {
                user: format!("user:{}", claims.sub),
                groups: claims.groups,
            },
            subject: claims.sub,
            token_id: claims.jti,
        })
    }

    async fn key(&self, kid: &str) -> Result<DecodingKey, proto::Error> {
        let keys = self.keys.lock().await;
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

    async fn refresh(&self) {
        let attempted = {
            let mut keys = self.keys.lock().await;
            let usable = keys.fetched.is_some_and(|at| at.elapsed() < TRUST_AT_MOST);
            let recent = keys.tried.is_some_and(|at| at.elapsed() < REFETCH_EVERY);
            if usable && recent {
                return;
            }
            let attempted = Instant::now();
            keys.tried = Some(attempted);
            attempted
        };
        match self.fetch().await {
            Ok(set) => {
                let mut keys = self.keys.lock().await;
                (keys.set, keys.fetched) = (set, Some(attempted));
            }
            Err(error) => tracing::warn!(%error, "fetching the issuer's keys"),
        }
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

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use axum::{Json, Router, http::StatusCode, routing::get};
    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;

    #[test]
    fn stale_issuer_keys_are_trusted_for_five_minutes() {
        // contract: S5
        assert_eq!(TRUST_AT_MOST, Duration::from_secs(300));
    }

    #[tokio::test]
    async fn a_slow_refresh_does_not_block_cached_keys() {
        // contract: S5
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let started = Arc::new(Notify::new());
        let signal = Arc::clone(&started);
        let discovery = get(move || {
            let signal = Arc::clone(&signal);
            async move {
                signal.notify_waiters();
                tokio::time::sleep(Duration::from_secs(1)).await;
                Json(json!({"jwks_uri": "http://127.0.0.1/unused"}))
            }
        });
        let issuer = Router::new().route("/.well-known/openid-configuration", discovery);
        tokio::spawn(async move { axum::serve(listener, issuer).await.unwrap() });
        let verifier = Arc::new(Verifier::new(url, "audience".into()));
        let set = serde_json::from_value(json!({"keys": [{
            "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "kid": "k1",
            "x": "1YXvpZn5rWDm4IkMDok-qEpwMr1Omjfvq7Gb0D3gI5I"
        }]}))
        .unwrap();
        let stale = Instant::now() - REFETCH_EVERY - Duration::from_secs(1);
        *verifier.keys.lock().await = Keys {
            set,
            tried: Some(stale),
            fetched: Some(stale),
        };

        let notified = started.notified();
        let refreshing = {
            let verifier = Arc::clone(&verifier);
            tokio::spawn(async move { verifier.refresh().await })
        };
        tokio::time::timeout(Duration::from_secs(1), notified)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), verifier.key("k1"))
            .await
            .expect("cached key was blocked by the refresh")
            .unwrap();
        refreshing.abort();
    }

    #[tokio::test]
    async fn unusable_keys_retry_immediately_after_a_failed_fetch() {
        // contract: S5
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let available = Arc::new(AtomicBool::new(false));
        let answers = Arc::clone(&available);
        let jwks_uri = format!("{url}/keys");
        let discovery = get(move || {
            let answers = Arc::clone(&answers);
            let jwks_uri = jwks_uri.clone();
            async move {
                answers
                    .load(Ordering::Relaxed)
                    .then(|| Json(json!({"jwks_uri": jwks_uri})))
                    .ok_or(StatusCode::SERVICE_UNAVAILABLE)
            }
        });
        let keys = get(|| async {
            Json(json!({"keys": [{
                "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "kid": "k1",
                "x": "1YXvpZn5rWDm4IkMDok-qEpwMr1Omjfvq7Gb0D3gI5I"
            }]}))
        });
        let issuer = Router::new()
            .route("/.well-known/openid-configuration", discovery)
            .route("/keys", keys);
        tokio::spawn(async move { axum::serve(listener, issuer).await.unwrap() });
        let verifier = Verifier::new(url, "audience".into());

        verifier.refresh().await;
        assert!(verifier.key("k1").await.is_err());
        available.store(true, Ordering::Relaxed);
        verifier.refresh().await;
        verifier.key("k1").await.unwrap();
    }
}

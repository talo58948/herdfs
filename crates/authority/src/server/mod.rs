//! The authority over HTTP: bearer auth, the RPC endpoint, chunk uploads,
//! presigned download URLs, and the store sweep after each collection.

mod auth;
mod sweep;

pub use sweep::Swept;

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Path, State,
        rejection::{BytesRejection, JsonRejection},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
};
use herdfs_core::{
    ChunkId, ChunkRef,
    chunk::MAX_CHUNK,
    proto::{self, Request, Response},
};
use rusty_s3::{Bucket, Credentials, S3Action};
use serde_json::json;
use tokio::sync::RwLock;

use crate::{Authority, LocalIssuer, Outcome, Principal, local::KEY_ID};

/// Commit bodies list every chunk: 32 MiB holds about 350,000, a 350 GiB file.
const RPC_BODY_LIMIT: usize = 32 << 20;
/// Uploads are written through at once, so the signed PUT and the request share this deadline.
const PUT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Server {
    authority: Mutex<Authority>,
    bucket: Bucket,
    public_bucket: Bucket,
    credentials: Credentials,
    http: reqwest::Client,
    verifier: auth::Verifier,
    local_key: Option<String>,
    /// How long a presigned chunk URL stays valid.
    grant_ttl: Duration,
    /// Held shared by each upload and alone by the sweep while it deletes, so
    /// the sweep never deletes bytes an upload has just written.
    uploading: RwLock<()>,
}

impl Server {
    /// Uploads go to `bucket`; download URLs name `public_bucket`, the same
    /// bucket as clients reach it. Tokens must come from `issuer` for `audience`.
    pub fn new(
        authority: Authority,
        bucket: Bucket,
        public_bucket: Bucket,
        credentials: Credentials,
        issuer: String,
        audience: String,
        grant_ttl: Duration,
    ) -> Self {
        Self {
            authority: Mutex::new(authority),
            bucket,
            public_bucket,
            credentials,
            http: reqwest::Client::new(),
            verifier: auth::Verifier::new(issuer, audience),
            local_key: None,
            grant_ttl,
            uploading: RwLock::new(()),
        }
    }

    /// Runs with the persisted built-in issuer and immediate token revocation.
    pub fn new_local(
        authority: Authority,
        bucket: Bucket,
        public_bucket: Bucket,
        credentials: Credentials,
        local_issuer: &LocalIssuer,
        grant_ttl: Duration,
    ) -> Self {
        let issuer = local_issuer.issuer().to_owned();
        let audience = local_issuer.audience().to_owned();
        let public_key = local_issuer.public_key();
        let mut server = Self::new(
            authority,
            bucket,
            public_bucket,
            credentials,
            issuer,
            audience,
            grant_ttl,
        );
        server.local_key = Some(public_key);
        server
    }

    pub fn router(self: &Arc<Self>) -> Router {
        let router = Router::new()
            .route("/v1/rpc", post(rpc).layer(DefaultBodyLimit::max(RPC_BODY_LIMIT)))
            .route(
                "/v1/spaces/{space}/commits/{request_id}/chunks/{id}",
                put(upload).layer(DefaultBodyLimit::max(MAX_CHUNK)),
            )
            .route("/v1/health", get(|| async { "ok" }));
        let Some(public_key) = self.local_key.clone() else {
            return router.with_state(Arc::clone(self));
        };
        let issuer = self.verifier.issuer().to_owned();
        let discovery = json!({
            "issuer": issuer.clone(),
            "jwks_uri": format!("{issuer}/.well-known/jwks.json"),
            "id_token_signing_alg_values_supported": ["EdDSA"]
        });
        let jwks = json!({"keys": [{
            "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig",
            "kid": KEY_ID, "x": public_key
        }]});
        let discovery = get(move || {
            let discovery = discovery.clone();
            async move { Json(discovery) }
        });
        let jwks = get(move || {
            let jwks = jwks.clone();
            async move { Json(jwks) }
        });
        let router = router
            .route("/.well-known/openid-configuration", discovery)
            .route("/.well-known/jwks.json", jwks);
        router.with_state(Arc::clone(self))
    }

    /// Runs `work` on the authority off the async runtime.
    async fn run<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&mut Authority, i64) -> Result<T, proto::Error> + Send + 'static,
    ) -> Result<T, proto::Error> {
        let server = Arc::clone(self);
        let task = tokio::task::spawn_blocking(move || {
            // A panic rolls back its transaction, so a poisoned authority is still consistent.
            let mut authority = server.authority.lock().unwrap_or_else(PoisonError::into_inner);
            work(&mut authority, now_ms())
        });
        task.await.map_err(unavailable)?
    }

    async fn principal(self: &Arc<Self>, headers: &HeaderMap) -> Result<Principal, proto::Error> {
        let authenticated = self.verifier.authenticate(headers).await?;
        if self.local_key.is_none() {
            return Ok(authenticated.principal);
        }
        let id = authenticated
            .token_id
            .ok_or_else(|| unauthenticated("local token has no id"))?;
        let user = authenticated.subject;
        let active = self
            .run(move |authority, now_ms| authority.check_local_token(&id, &user, now_ms))
            .await?;
        if !active {
            return Err(unauthenticated("local token is expired or revoked"));
        }
        Ok(authenticated.principal)
    }

    fn sign(&self, chunks: &[ChunkId]) -> Vec<String> {
        let url = |id| {
            self.public_bucket
                .get_object(Some(&self.credentials), &chunk_key(id))
                .sign(self.grant_ttl)
        };
        chunks.iter().map(|&id| url(id).into()).collect()
    }

    /// Writes a chunk to the store, whether or not it is there already.
    async fn put(&self, id: ChunkId, bytes: Bytes) -> Result<(), proto::Error> {
        let url = self
            .bucket
            .put_object(Some(&self.credentials), &chunk_key(id))
            .sign(PUT_TIMEOUT);
        let reply = self.http.put(url).timeout(PUT_TIMEOUT).body(bytes).send().await;
        reply
            .and_then(reqwest::Response::error_for_status)
            .map_err(unavailable)?;
        Ok(())
    }
}

async fn rpc(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Result<Json<Request>, JsonRejection>,
) -> Result<Json<Response>, Failure> {
    let who = server.principal(&headers).await?;
    let Json(request) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    let outcome = server
        .run(move |authority, now_ms| authority.handle(&who, request, now_ms))
        .await?;
    let reply = match outcome {
        Outcome::Reply(reply) => reply,
        Outcome::Sign { chunks } => Response::Grants {
            urls: server.sign(&chunks),
            ttl_ms: u64::try_from(server.grant_ttl.as_millis()).unwrap_or(u64::MAX),
        },
    };
    Ok(Json(reply))
}

/// Stores a chunk even if it already exists, so an upload never reveals
/// whether the same bytes are stored elsewhere. Checking first means a caller
/// without write causes no store write; recording only once the store holds
/// the bytes means no commit can name a chunk it lacks (C4b). If the write
/// fails, the caller gets `Unavailable` and uploads again.
async fn upload(
    State(server): State<Arc<Server>>,
    Path((space, request_id, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Response>, Failure> {
    let who = server.principal(&headers).await?;
    let id: ChunkId = id.parse().map_err(|_| invalid(format!("bad chunk id {id:?}")))?;
    let bytes = body.map_err(|rejection| invalid(rejection.body_text()))?;
    if ChunkId::of(&bytes) != id {
        return Err(invalid(format!("body does not hash to {id}")).into());
    }
    let len = u32::try_from(bytes.len()).map_err(|_| invalid("chunk too large"))?;
    let chunk = ChunkRef { id, len };
    let _uploading = server.uploading.read().await;
    let (checked, space_checked) = (who.clone(), space.clone());
    server
        .run(move |authority, now_ms| authority.check_upload(&checked, &space_checked, now_ms))
        .await?;
    server.put(id, bytes).await?;
    server
        .run(move |authority, now_ms| authority.record_upload(&who, &space, &request_id, chunk, now_ms))
        .await?;
    Ok(Json(Response::Done))
}

fn chunk_key(id: ChunkId) -> String {
    format!("chunks/{id}")
}

fn now_ms() -> i64 {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

/// A protocol error as an HTTP response.
struct Failure(proto::Error);

impl From<proto::Error> for Failure {
    fn from(error: proto::Error) -> Self {
        Self(error)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> axum::response::Response {
        let status = match self.0.code {
            proto::ErrorCode::NotFound => StatusCode::NOT_FOUND,
            proto::ErrorCode::Exists
            | proto::ErrorCode::NotEmpty
            | proto::ErrorCode::NotDir
            | proto::ErrorCode::IsDir
            | proto::ErrorCode::Conflict => StatusCode::CONFLICT,
            proto::ErrorCode::Denied => StatusCode::FORBIDDEN,
            proto::ErrorCode::Unauthenticated => StatusCode::UNAUTHORIZED,
            proto::ErrorCode::Invalid => StatusCode::BAD_REQUEST,
            proto::ErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, Json(self.0)).into_response()
    }
}

fn invalid(message: impl Into<String>) -> proto::Error {
    proto::Error::new(proto::ErrorCode::Invalid, message)
}

fn unauthenticated(message: impl Into<String>) -> proto::Error {
    proto::Error::new(proto::ErrorCode::Unauthenticated, message)
}

fn unavailable(error: impl std::fmt::Display) -> proto::Error {
    tracing::warn!(%error, "unavailable");
    proto::Error::new(proto::ErrorCode::Unavailable, "service unavailable")
}

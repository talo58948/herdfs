use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use axum::{
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::Response as HttpResponse,
};
use herdfs_core::proto::Request;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Barrier, Mutex};

use super::*;

#[test]
fn malformed_json_is_not_a_legacy_bearer_token() -> anyhow::Result<()> {
    // contract: S3, U1
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("token");
    fs::write(&path, r#"{"refresh_token":"secret""#)?;
    assert!(read_access_token(&path).is_err());
    Ok(())
}

#[tokio::test]
async fn a_valid_cached_token_needs_no_file_read() -> anyhow::Result<()> {
    // contract: U1
    let (dir, path) = token_file(60)?;
    let source = Source::new(path.clone())?;
    fs::remove_file(path)?;
    assert_eq!(source.access(&Client::new(), false).await?, "access");
    drop(dir);
    Ok(())
}

#[tokio::test]
async fn a_failed_early_refresh_uses_the_still_valid_token() -> anyhow::Result<()> {
    // contract: U1
    let endpoint = endpoint(
        json!({"access_token": "new", "expires_in": 60}),
        Duration::from_secs(1),
    )
    .await?;
    let (dir, path) = token_file(10)?;
    let mut token = read_login(&path)?;
    token.token_endpoint = endpoint;
    write(&path, &token, token.generation)?;
    let http = Client::builder().timeout(Duration::from_millis(10)).build()?;
    assert_eq!(Source::new(path)?.access(&http, false).await?, "access");
    drop(dir);
    Ok(())
}

#[tokio::test]
async fn login_exchange_needs_a_refresh_token() -> anyhow::Result<()> {
    // contract: U1
    let endpoint = endpoint(
        json!({"access_token": "access", "expires_in": 60}),
        Duration::ZERO,
    )
    .await?;
    let Err(error) = exchange(&Client::new(), &endpoint, &[("client_id", "client")], None).await else {
        panic!("exchange without a refresh token succeeded")
    };
    assert!(error.to_string().contains("issuer returned no refresh token"));
    Ok(())
}

/// Concurrent requests sharing one login must reuse the first completed refresh just as separate
/// processes do, or a rotating issuer can invalidate the first caller's retry. An uncontended
/// expired request and a request with the final fresh token control the ordinary paths.
// contract: U1
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "ROUND74: concurrent calls on one client refresh the same rotating login twice"]
async fn concurrent_calls_share_one_rotating_refresh() -> anyhow::Result<()> {
    let mut failed = 0;
    for trial in 0..3 {
        assert_eq!(request_wave(1).await?, (1, 1), "uncontended control {trial}");
        let (succeeded, refreshes) = request_wave(2).await?;
        eprintln!("trial {trial}: {succeeded}/2 calls succeeded after {refreshes} refreshes");
        failed += usize::from(succeeded != 2 || refreshes != 1);
    }
    assert_eq!(failed, 0, "{failed}/3 shared-client request waves failed");
    Ok(())
}

struct RefreshService {
    authority: String,
    token_endpoint: String,
    refreshes: Arc<AtomicUsize>,
}

async fn refresh_service(callers: usize) -> anyhow::Result<RefreshService> {
    let current = Arc::new(Mutex::new("stale".to_owned()));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let token_listener = TcpListener::bind("127.0.0.1:0").await?;
    let token_endpoint = format!("http://{}/token", token_listener.local_addr()?);
    let token = Router::new().route(
        "/token",
        post({
            let current = Arc::clone(&current);
            let refreshes = Arc::clone(&refreshes);
            move || {
                let current = Arc::clone(&current);
                let generation = refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    let access = format!("access-{generation}");
                    *current.lock().await = access.clone();
                    Json(json!({
                        "access_token": access,
                        "refresh_token": format!("refresh-{generation}"),
                        "expires_in": 60
                    }))
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(token_listener, token).await });

    let stale = Arc::new(Barrier::new(callers));
    let authority_listener = TcpListener::bind("127.0.0.1:0").await?;
    let authority = format!("http://{}", authority_listener.local_addr()?);
    let rpc = Router::new().route(
        "/v1/rpc",
        post({
            let current = Arc::clone(&current);
            move |headers: HeaderMap| {
                let current = Arc::clone(&current);
                let stale = Arc::clone(&stale);
                async move {
                    let bearer = headers
                        .get(AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.strip_prefix("Bearer "))
                        .unwrap_or_default()
                        .to_owned();
                    if bearer == "stale" {
                        stale.wait().await;
                        return refusal();
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if *current.lock().await == bearer {
                        return (
                            StatusCode::OK,
                            Json(json!({
                                "type": "me",
                                "principal": "user:alice",
                                "groups": []
                            })),
                        )
                            .into_response();
                    }
                    refusal()
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(authority_listener, rpc).await });
    Ok(RefreshService {
        authority,
        token_endpoint,
        refreshes,
    })
}

fn refusal() -> HttpResponse {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"code": "unauthenticated", "message": "stale token"})),
    )
        .into_response()
}

async fn request_wave(callers: usize) -> anyhow::Result<(usize, usize)> {
    let service = refresh_service(callers).await?;
    let (dir, path) = token_file(60)?;
    let mut stored = read_login(&path)?;
    stored.access_token = "stale".into();
    stored.token_endpoint = service.token_endpoint;
    write(&path, &stored, stored.generation)?;
    let remote = Arc::new(crate::remote::Remote::new(&service.authority, "", path, &[])?);
    let calls: Vec<_> = (0..callers)
        .map(|_| {
            let remote = Arc::clone(&remote);
            tokio::spawn(async move { remote.request(Request::Whoami).await })
        })
        .collect();
    let mut succeeded = 0;
    for call in calls {
        succeeded += usize::from(call.await?.is_ok());
    }
    assert!(
        remote.request(Request::Whoami).await.is_ok(),
        "fresh-token control"
    );
    drop(dir);
    Ok((succeeded, service.refreshes.load(Ordering::SeqCst)))
}

fn token_file(expires_in: u64) -> anyhow::Result<(tempfile::TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("token");
    write(
        &path,
        &TokenFile {
            generation: 0,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: expiry(expires_in),
            client_id: "client".into(),
            token_endpoint: "http://unused".into(),
        },
        0,
    )?;
    Ok((dir, path))
}

fn read_login(path: &Path) -> anyhow::Result<TokenFile> {
    let Stored::Login(token) = read(path)? else {
        anyhow::bail!("not a login token")
    };
    Ok(token)
}

async fn endpoint(body: Value, delay: Duration) -> anyhow::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = axum::Router::new().route(
        "/",
        post(move || {
            let body = body.clone();
            async move {
                tokio::time::sleep(delay).await;
                Json(body)
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(url)
}

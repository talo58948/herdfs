use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;

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
/// processes do; an uncontended forced refresh controls the ordinary path.
// contract: U1
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "ROUND74: concurrent calls on one client refresh the same rotating login twice"]
async fn concurrent_calls_share_one_rotating_refresh() -> anyhow::Result<()> {
    let mut duplicated = 0;
    for trial in 0..3 {
        assert_eq!(
            forced_refreshes(1).await?,
            (vec!["access-1".to_owned()], 1),
            "uncontended control {trial}"
        );
        let (mut access, count) = forced_refreshes(2).await?;
        access.sort();
        eprintln!("trial {trial}: returned {access:?} after {count} refreshes");
        duplicated += usize::from(access != ["access-1", "access-1"] || count != 1);
    }
    assert_eq!(duplicated, 0, "{duplicated}/3 shared clients refreshed twice");
    Ok(())
}

async fn forced_refreshes(callers: usize) -> anyhow::Result<(Vec<String>, usize)> {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let app = axum::Router::new().route(
        "/",
        post({
            let refreshes = Arc::clone(&refreshes);
            move || {
                let generation = refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    Json(json!({
                        "access_token": format!("access-{generation}"),
                        "refresh_token": format!("refresh-{generation}"),
                        "expires_in": 60
                    }))
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await });

    let (dir, path) = token_file(60)?;
    let mut stored = read_login(&path)?;
    stored.token_endpoint = endpoint;
    write(&path, &stored, stored.generation)?;
    let source = Arc::new(Source::new(path)?);
    let http = Client::new();
    let calls: Vec<_> = (0..callers)
        .map(|_| {
            let source = Arc::clone(&source);
            let http = http.clone();
            tokio::spawn(async move { source.access(&http, true).await })
        })
        .collect();
    let mut access = Vec::new();
    for call in calls {
        access.push(call.await??);
    }
    drop(dir);
    Ok((access, refreshes.load(Ordering::SeqCst)))
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

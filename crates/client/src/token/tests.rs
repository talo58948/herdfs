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

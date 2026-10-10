//! OIDC authorization code login with PKCE and a loopback callback.

use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap::Args;
use futures::{StreamExt, stream::FuturesUnordered};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::TokenArgs;
use crate::token;

#[derive(Args)]
pub(super) struct LoginArgs {
    /// OIDC issuer URL.
    #[arg(long)]
    issuer: String,
    /// Public OIDC client id.
    #[arg(long)]
    client_id: String,
    /// Loopback callback port; 0 asks the operating system for a random port.
    #[arg(long, default_value_t = 53682)]
    redirect_port: u16,
    #[command(flatten)]
    token: TokenArgs,
}

#[derive(Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

pub(super) async fn login(args: &LoginArgs, json: bool) -> anyhow::Result<()> {
    let token_file = args.token.path()?;
    let issuer = args.issuer.trim_end_matches('/');
    let discovery = reqwest::get(format!("{issuer}/.well-known/openid-configuration"))
        .await?
        .error_for_status()?
        .json::<Discovery>()
        .await?;
    let listener = redirect_listener(args.redirect_port).await?;
    let redirect = format!("http://{}/callback", listener.local_addr()?);
    let verifier = random_urlsafe();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe();
    let mut authorize = reqwest::Url::parse(&discovery.authorization_endpoint)?;
    authorize.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", args.client_id.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("scope", "openid offline_access"),
        ("state", state.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ]);
    if json {
        eprintln!("{authorize}");
    } else {
        println!("{authorize}");
        io::stdout().flush()?;
    }
    open_browser(authorize.as_str());
    let (mut callback, code) = tokio::time::timeout(Duration::from_secs(300), callback(&listener, &state))
        .await
        .context("timed out waiting for the OIDC callback")??;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", args.client_id.as_str()),
        ("code_verifier", verifier.as_str()),
    ];
    let http = reqwest::Client::new();
    let token = token::exchange(&http, &discovery.token_endpoint, &form, None).await?;
    token::save(&token_file, &token).await?;
    reply(&mut callback, "200 OK", "herdfs signed in\n").await?;
    if json {
        super::output::json(&serde_json::json!({"token_file": token_file}))?;
    } else {
        println!("signed in");
    }
    Ok(())
}

async fn redirect_listener(port: u16) -> anyhow::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port)).await.with_context(|| {
        format!(
            "OIDC redirect port {port} is unavailable; stop the process using it, choose --redirect-port PORT, or use --redirect-port 0 for a random port"
        )
    })
}

async fn callback(
    listener: &TcpListener,
    expected_state: &str,
) -> anyhow::Result<(tokio::net::TcpStream, String)> {
    let mut requests = FuturesUnordered::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                requests.push(read_callback(stream, expected_state));
            }
            request = requests.next(), if !requests.is_empty() => {
                let Some(request) = request else { continue };
                if let Some(callback) = request? {
                    return Ok(callback);
                }
            }
        }
    }
}

async fn read_callback(
    mut stream: tokio::net::TcpStream,
    expected_state: &str,
) -> anyhow::Result<Option<(tokio::net::TcpStream, String)>> {
    let mut request = vec![0; 8192];
    let mut len = 0;
    let header_end = loop {
        if let Some(end) = request[..len].windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break Some(end + 4);
        }
        if len == request.len() {
            break None;
        }
        let read = stream.read(&mut request[len..]).await?;
        if read == 0 {
            break None;
        }
        len += read;
    };
    let Some(header_end) = header_end else {
        reply(&mut stream, "400 Bad Request", "bad callback request\n").await?;
        return Ok(None);
    };
    let request = std::str::from_utf8(&request[..header_end])?;
    let Some(target) = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
    else {
        reply(&mut stream, "400 Bad Request", "bad callback request\n").await?;
        return Ok(None);
    };
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))?;
    if url.path() != "/callback" {
        reply(&mut stream, "404 Not Found", "not found\n").await?;
        return Ok(None);
    }
    let parameter = |name| {
        url.query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
    };
    if parameter("state").as_deref() != Some(expected_state) {
        reply(&mut stream, "400 Bad Request", "OIDC state did not match\n").await?;
        return Ok(None);
    }
    if let Some(error) = parameter("error") {
        let description = parameter("error_description");
        reply(&mut stream, "400 Bad Request", "sign in failed\n").await?;
        bail!(
            "issuer returned {error}{}",
            description.map_or(String::new(), |text| format!(": {text}"))
        );
    }
    let Some(code) = parameter("code") else {
        reply(&mut stream, "400 Bad Request", "OIDC callback lacked a code\n").await?;
        return Ok(None);
    };
    Ok(Some((stream, code)))
}

async fn reply(stream: &mut tokio::net::TcpStream, status: &str, body: &str) -> anyhow::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

fn random_urlsafe() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(error) = Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        tracing::warn!(%error, "could not open the browser; use the printed URL");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn callback_ignores_other_requests_and_the_wrong_state() -> anyhow::Result<()> {
        // contract: U1
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let send = async move {
            for target in [
                "/favicon.ico",
                "/callback?code=bad&state=wrong",
                "/callback?code=code&state=right",
            ] {
                let mut stream = TcpStream::connect(address).await?;
                stream
                    .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
                    .await?;
            }
            anyhow::Ok(())
        };
        let (received, sent) = tokio::join!(callback(&listener, "right"), send);
        sent?;
        assert_eq!(received?.1, "code");
        Ok(())
    }

    #[tokio::test]
    async fn callback_surfaces_the_issuers_error() -> anyhow::Result<()> {
        // contract: U1
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut stream = TcpStream::connect(listener.local_addr()?).await?;
        let send = stream.write_all(
            b"GET /callback?error=access_denied&error_description=cancelled&state=right HTTP/1.1\r\n\r\n",
        );
        let (received, sent) = tokio::join!(callback(&listener, "right"), send);
        sent?;
        let Err(error) = received else {
            panic!("issuer error callback succeeded")
        };
        assert!(error.to_string().contains("access_denied: cancelled"));
        Ok(())
    }

    #[tokio::test]
    async fn a_busy_redirect_port_has_a_clear_error() -> anyhow::Result<()> {
        // contract: U1
        let occupied = TcpListener::bind("127.0.0.1:0").await?;
        let port = occupied.local_addr()?.port();
        let Err(error) = redirect_listener(port).await else {
            panic!("two listeners bound redirect port {port}")
        };
        let message = error.to_string();
        assert!(message.contains(&port.to_string()), "{message}");
        assert!(message.contains("--redirect-port 0"), "{message}");
        Ok(())
    }
}

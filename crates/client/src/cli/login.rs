//! OIDC authorization code login with PKCE and a loopback callback.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, ensure};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap::Args;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::token::{self, TokenFile};

#[derive(Args)]
pub(super) struct LoginArgs {
    /// OIDC issuer URL.
    #[arg(long)]
    issuer: String,
    /// Public OIDC client id.
    #[arg(long)]
    client_id: String,
    /// Login token file.
    #[arg(long, default_value_os_t = token::default_path())]
    token_file: PathBuf,
}

#[derive(Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

pub(super) async fn login(args: &LoginArgs, json: bool) -> anyhow::Result<()> {
    let issuer = args.issuer.trim_end_matches('/');
    let discovery = reqwest::get(format!("{issuer}/.well-known/openid-configuration"))
        .await?
        .error_for_status()?
        .json::<Discovery>()
        .await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!("http://{}/callback", listener.local_addr()?);
    let verifier = random_urlsafe(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(24);
    let mut authorize = reqwest::Url::parse(&discovery.authorization_endpoint)?;
    authorize.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", args.client_id.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("scope", "openid"),
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
    let code = callback(&listener, &state).await?;
    let response = reqwest::Client::new()
        .post(&discovery.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("client_id", args.client_id.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json::<TokenResponse>()
        .await?;
    token::write(
        &args.token_file,
        &TokenFile {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at: token::expiry(response.expires_in),
            issuer: issuer.to_owned(),
            client_id: args.client_id.clone(),
            token_endpoint: discovery.token_endpoint,
        },
    )?;
    if json {
        super::output::json(&serde_json::json!({"token_file": args.token_file}))?;
    } else {
        println!("signed in");
    }
    Ok(())
}

async fn callback(listener: &TcpListener, expected_state: &str) -> anyhow::Result<String> {
    let (mut stream, _) = listener.accept().await?;
    let mut request = vec![0; 8192];
    let read = stream.read(&mut request).await?;
    let request = std::str::from_utf8(&request[..read])?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("loopback callback lacked a request target")?;
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))?;
    let parameter = |name| {
        url.query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
    };
    let state = parameter("state").context("OIDC callback lacked state")?;
    ensure!(state == expected_state, "OIDC state did not match");
    let code = parameter("code").context("OIDC callback lacked a code")?;
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 20\r\n\r\nherdfs signed in\n",
        )
        .await?;
    Ok(code)
}

fn random_urlsafe(bytes: usize) -> String {
    let bytes: Vec<u8> = (0..bytes).map(|_| rand::random()).collect();
    URL_SAFE_NO_PAD.encode(bytes)
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "linux")]
    let opener = "xdg-open";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let opener = "open";
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
    async fn callback_rejects_the_wrong_state() -> anyhow::Result<()> {
        // contract: U1
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let send = async move {
            let mut stream = TcpStream::connect(address).await?;
            stream
                .write_all(b"GET /callback?code=code&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
        };
        let (received, sent) = tokio::join!(callback(&listener, "right"), send);
        sent?;
        assert!(received.is_err());
        Ok(())
    }
}

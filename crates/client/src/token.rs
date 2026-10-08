//! The login token file and refreshes shared by mounts and direct commands.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use reqwest::Client;
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{Error, Result};

const REFRESH_MARGIN: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize)]
pub(crate) struct TokenFile {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    client_id: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

pub(crate) struct Source {
    path: PathBuf,
    stored: Mutex<Stored>,
}

impl Source {
    pub(crate) fn new(path: PathBuf) -> Result<Self> {
        let stored = read(&path)?;
        Ok(Self {
            path,
            stored: Mutex::new(stored),
        })
    }

    pub(crate) async fn access(&self, http: &Client, force_refresh: bool) -> Result<String> {
        let mut stored = self.stored.lock().await;
        let cached_access = match &*stored {
            Stored::Legacy(_) if !force_refresh => return stored.access(),
            Stored::Legacy(token) => token.clone(),
            Stored::Login(token) if !force_refresh && !refresh_due(token) => {
                return Ok(token.access_token.clone());
            }
            Stored::Login(token) => token.access_token.clone(),
        };
        let _lock = lock(self.path.with_extension("lock")).await?;
        *stored = read(&self.path)?;
        let Stored::Login(token) = &*stored else {
            return stored.access();
        };
        if (!force_refresh && !refresh_due(token)) || (force_refresh && token.access_token != cached_access) {
            return Ok(token.access_token.clone());
        }
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", token.refresh_token.as_str()),
            ("client_id", token.client_id.as_str()),
        ];
        let refreshed = exchange(http, &token.token_endpoint, &form, Some(&token.refresh_token)).await;
        let refreshed = match refreshed {
            Ok(refreshed) => refreshed,
            Err(_) if !force_refresh && now() < token.expires_at => {
                return Ok(token.access_token.clone());
            }
            Err(error) => return Err(error),
        };
        write(&self.path, &refreshed)?;
        let access = refreshed.access_token.clone();
        *stored = Stored::Login(refreshed);
        Ok(access)
    }
}

enum Stored {
    Login(TokenFile),
    Legacy(String),
}

impl Stored {
    fn access(&self) -> Result<String> {
        match self {
            Self::Login(token) => Ok(token.access_token.clone()),
            Self::Legacy(token) => Ok(token.clone()),
        }
    }
}

fn read(path: &Path) -> Result<Stored> {
    let text = fs::read_to_string(path)?;
    match serde_json::from_str(&text) {
        Ok(token) => Ok(Stored::Login(token)),
        Err(_) if !text.trim_start().starts_with('{') => Ok(Stored::Legacy(text.trim().to_owned())),
        Err(error) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error).into()),
    }
}

pub(crate) fn read_access_token(path: &Path) -> Result<String> {
    match read(path)? {
        Stored::Login(token) => Ok(token.access_token),
        Stored::Legacy(token) => Ok(token),
    }
}

pub(crate) fn write(path: &Path, token: &TokenFile) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, token).map_err(std::io::Error::other)?;
    temporary.write_all(b"\n")?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub(crate) fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

pub(crate) fn default_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is unset; use --token-file")?;
    Ok(PathBuf::from(home).join(".config/herdfs/token"))
}

fn expiry(expires_in: u64) -> u64 {
    now().saturating_add(expires_in)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn refresh_due(token: &TokenFile) -> bool {
    now() >= token.expires_at.saturating_sub(REFRESH_MARGIN.as_secs())
}

async fn lock(path: PathBuf) -> Result<File> {
    Ok(tokio::task::spawn_blocking(move || -> std::io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        flock(&file, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
        Ok(file)
    })
    .await
    .map_err(std::io::Error::other)??)
}

pub(crate) async fn exchange(
    http: &Client,
    endpoint: &str,
    form: &[(&str, &str)],
    previous_refresh: Option<&str>,
) -> Result<TokenFile> {
    let response = http
        .post(endpoint)
        .form(form)
        .send()
        .await?
        .error_for_status()?
        .json::<TokenResponse>()
        .await?;
    let refresh_token = response
        .refresh_token
        .or_else(|| previous_refresh.map(str::to_owned))
        .ok_or_else(|| Error::Protocol("issuer returned no refresh token".into()))?;
    let client_id = form
        .iter()
        .find_map(|(name, value)| (*name == "client_id").then(|| (*value).to_owned()))
        .ok_or_else(|| Error::Protocol("token exchange omitted client_id".into()))?;
    Ok(TokenFile {
        access_token: response.access_token,
        refresh_token,
        expires_at: expiry(response.expires_in),
        client_id,
        token_endpoint: endpoint.to_owned(),
    })
}

#[cfg(test)]
mod tests;

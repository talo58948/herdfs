//! The login token file and refreshes shared by mounts and direct commands.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{Error, Result};

const REFRESH_MARGIN: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TokenFile {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub issuer: String,
    pub client_id: String,
    pub token_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

pub(crate) struct Source {
    path: PathBuf,
    refreshing: Mutex<()>,
}

impl Source {
    pub(crate) fn new(path: PathBuf) -> Result<Self> {
        read_access_token(&path)?;
        Ok(Self {
            path,
            refreshing: Mutex::new(()),
        })
    }

    pub(crate) async fn access(&self, force_refresh: bool) -> Result<String> {
        let _refreshing = self.refreshing.lock().await;
        let Stored::Login(mut token) = read(&self.path)? else {
            return read_access_token(&self.path);
        };
        let refresh_at = token.expires_at.saturating_sub(REFRESH_MARGIN.as_secs());
        if !force_refresh && now() < refresh_at {
            return Ok(token.access_token);
        }
        let response = reqwest::Client::new()
            .post(&token.token_endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", token.refresh_token.as_str()),
                ("client_id", token.client_id.as_str()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json::<TokenResponse>()
            .await?;
        token.access_token = response.access_token;
        token.expires_at = now().saturating_add(response.expires_in);
        if let Some(refresh_token) = response.refresh_token {
            token.refresh_token = refresh_token;
        }
        write(&self.path, &token)?;
        Ok(token.access_token)
    }
}

enum Stored {
    Login(TokenFile),
    Legacy(String),
}

fn read(path: &Path) -> Result<Stored> {
    let text = fs::read_to_string(path)?;
    match serde_json::from_str(&text) {
        Ok(token) => Ok(Stored::Login(token)),
        Err(_) => Ok(Stored::Legacy(text.trim().to_owned())),
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
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o600))?;
    fs::rename(temporary.path(), path)?;
    Ok(())
}

pub(crate) fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

pub fn default_path() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".config/herdfs/token"),
        |home| PathBuf::from(home).join(".config/herdfs/token"),
    )
}

pub(crate) fn expiry(expires_in: u64) -> u64 {
    now().saturating_add(expires_in)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

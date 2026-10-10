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

mod exchange;
pub(crate) use exchange::exchange;

const REFRESH_MARGIN: Duration = Duration::from_secs(30);

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TokenFile {
    #[serde(default)]
    generation: u64,
    access_token: String,
    #[cfg_attr(target_os = "macos", serde(default, skip_serializing))]
    refresh_token: String,
    expires_at: u64,
    client_id: String,
    token_endpoint: String,
}

pub(crate) struct Source {
    path: PathBuf,
    stored: Mutex<Stored>,
}

type Access = (String, Option<u64>);

impl Source {
    pub(crate) fn new(path: PathBuf) -> Result<Self> {
        let stored = read(&path)?;
        Ok(Self {
            path,
            stored: Mutex::new(stored),
        })
    }

    pub(crate) async fn access(&self, http: &Client) -> Result<Access> {
        self.token(http, None).await
    }

    pub(crate) async fn refresh(&self, http: &Client, generation: Option<u64>) -> Result<Access> {
        self.token(http, Some(generation)).await
    }

    async fn token(&self, http: &Client, refresh_after: Option<Option<u64>>) -> Result<Access> {
        let mut stored = self.stored.lock().await;
        match &*stored {
            Stored::Legacy(_) if refresh_after.is_none() => return Ok(stored.access()),
            Stored::Legacy(_) => {}
            Stored::Login(token) if refresh_after.is_none() && !refresh_due(token) => {
                return Ok(stored.access());
            }
            Stored::Login(_) => {}
        }
        let _lock = lock(self.path.with_extension("lock")).await?;
        *stored = read(&self.path)?;
        let Stored::Login(token) = &*stored else {
            return Ok(stored.access());
        };
        if refresh_after.is_none() && !refresh_due(token)
            || refresh_after == Some(None)
            || refresh_after
                .flatten()
                .is_some_and(|generation| token.generation > generation)
        {
            return Ok(stored.access());
        }
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", token.refresh_token.as_str()),
            ("client_id", token.client_id.as_str()),
        ];
        let expires_at = token.expires_at;
        let generation = token.generation;
        let refreshed = exchange(http, &token.token_endpoint, &form, Some(&token.refresh_token)).await;
        let mut refreshed = match refreshed {
            Ok(refreshed) => refreshed,
            Err(_) if refresh_after.is_none() && now() < expires_at => {
                return Ok(stored.access());
            }
            Err(error) => return Err(error),
        };
        refreshed.generation = write(&self.path, &refreshed, generation)?;
        *stored = Stored::Login(refreshed);
        Ok(stored.access())
    }
}

enum Stored {
    Login(TokenFile),
    Legacy(String),
}

impl Stored {
    fn access(&self) -> Access {
        match self {
            Self::Login(token) => (token.access_token.clone(), Some(token.generation)),
            Self::Legacy(token) => (token.clone(), None),
        }
    }
}

fn read(path: &Path) -> Result<Stored> {
    let text = fs::read_to_string(path)?;
    match serde_json::from_str(&text) {
        Ok(mut token) => {
            load_refresh(path, &mut token)?;
            Ok(Stored::Login(token))
        }
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

fn write(path: &Path, token: &TokenFile, generation: u64) -> Result<u64> {
    let mut token = token.clone();
    token.generation = generation
        .checked_add(1)
        .ok_or_else(|| Error::Protocol("token generation exhausted".into()))?;
    store_refresh(path, &token.refresh_token)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, &token).map_err(std::io::Error::other)?;
    temporary.write_all(b"\n")?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(token.generation)
}

pub(crate) async fn save(path: &Path, token: &TokenFile) -> Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let _lock = lock(path.with_extension("lock")).await?;
    write(path, token, generation(path)).map(drop)
}

fn generation(path: &Path) -> u64 {
    serde_json::from_slice::<TokenFile>(&fs::read(path).unwrap_or_default())
        .map_or(0, |token| token.generation)
}

pub(crate) fn remove(path: &Path) -> Result<()> {
    remove_refresh(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

#[cfg(target_os = "macos")]
fn load_refresh(path: &Path, token: &mut TokenFile) -> Result<()> {
    use security_framework::passwords::get_generic_password;

    if !token.refresh_token.is_empty() {
        token.generation = write(path, token, token.generation)?;
        return Ok(());
    }
    let bytes = get_generic_password("herdfs", &path.to_string_lossy())
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    token.refresh_token = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn load_refresh(_path: &Path, _token: &mut TokenFile) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn store_refresh(path: &Path, refresh: &str) -> Result<()> {
    security_framework::passwords::set_generic_password(
        "herdfs",
        &path.to_string_lossy(),
        refresh.as_bytes(),
    )
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn store_refresh(_path: &Path, _refresh: &str) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_refresh(path: &Path) -> Result<()> {
    const ITEM_NOT_FOUND: i32 = -25_300;
    match security_framework::passwords::delete_generic_password("herdfs", &path.to_string_lossy()) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == ITEM_NOT_FOUND => Ok(()),
        Err(error) => Err(std::io::Error::other(error.to_string()).into()),
    }
}

#[cfg(not(target_os = "macos"))]
fn remove_refresh(_path: &Path) -> Result<()> {
    Ok(())
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

#[cfg(test)]
mod tests;

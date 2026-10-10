use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Registration {
    pub space: String,
    pub authority: String,
    pub mountpoint: PathBuf,
    pub socket: PathBuf,
    pub pid: u32,
}

pub(super) struct Registered {
    path: PathBuf,
}

impl Registered {
    pub fn write(registration: &Registration) -> anyhow::Result<Self> {
        let dir = directory()?;
        fs::create_dir_all(&dir)?;
        let key = Sha256::digest(registration.mountpoint.as_os_str().as_bytes());
        let path = dir.join(format!("{key:x}.json"));
        let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
        serde_json::to_writer(&mut temp, registration)?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| error.error)?;
        fs::File::open(&dir)?.sync_all()?;
        Ok(Self { path })
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn find_mount(path: &Path) -> anyhow::Result<Registration> {
    let path = canonical_path(path)?;
    registrations()?
        .into_iter()
        .filter(|mount| path.starts_with(&mount.mountpoint))
        .max_by_key(|mount| mount.mountpoint.as_os_str().len())
        .with_context(|| format!("{} is not inside a registered herdfs mount", path.display()))
}

pub(crate) fn only_mount(path: Option<&Path>) -> anyhow::Result<Registration> {
    if let Some(path) = path {
        return find_mount(path);
    }
    let mounts = registrations()?;
    ensure!(
        mounts.len() == 1,
        "expected one mounted space, found {}",
        mounts.len()
    );
    mounts.into_iter().next().context("mounted space disappeared")
}

fn registrations() -> anyhow::Result<Vec<Registration>> {
    let dir = directory()?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut mounts = Vec::new();
    let mut mountpoints = HashSet::new();
    for entry in entries {
        let path = match entry {
            Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "json") => entry.path(),
            Ok(_) => continue,
            Err(error) => {
                tracing::warn!(%error, "could not read a mount registration entry");
                continue;
            }
        };
        let registration = fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))
            .and_then(|bytes| {
                serde_json::from_slice::<Registration>(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))
            });
        let mount = match registration {
            Ok(mount) => mount,
            Err(error) => {
                tracing::warn!(%error, "skipping unreadable mount registration");
                continue;
            }
        };
        if UnixStream::connect(&mount.socket).is_ok() && mountpoints.insert(mount.mountpoint.clone()) {
            mounts.push(mount);
        }
    }
    Ok(mounts)
}

pub(crate) fn canonical_path(path: &Path) -> anyhow::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let Some(name) = path.file_name() else {
        return Ok(fs::canonicalize(path)?);
    };
    let parent = path.parent().context("path has no parent")?;
    Ok(fs::canonicalize(parent)?.join(name))
}

fn directory() -> anyhow::Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(config.join("herdfs/mounts"))
}

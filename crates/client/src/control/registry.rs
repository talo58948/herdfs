use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Registration {
    pub id: String,
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
        let path = dir.join(format!("{}.json", registration.id));
        let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
        serde_json::to_writer(&mut temp, registration)?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| error.error)?;
        Ok(Self { path })
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn find_mount(path: &Path) -> anyhow::Result<Registration> {
    let path = super::absolute(path)?;
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
    ensure!(mounts.len() == 1, "expected one mounted space, found {}", mounts.len());
    Ok(mounts.into_iter().next().expect("length checked above"))
}

fn registrations() -> anyhow::Result<Vec<Registration>> {
    let dir = directory()?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    entries
        .filter_map(|entry| match entry {
            Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "json") => Some(entry.path()),
            _ => None,
        })
        .map(|path| {
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_slice::<Registration>(&bytes)
                .with_context(|| format!("parsing {}", path.display()))
        })
        .filter(|registration| match registration {
            Ok(mount) => process_exists(mount.pid),
            Err(_) => true,
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    // SAFETY: signal 0 checks process existence without sending a signal.
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

fn directory() -> anyhow::Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(config.join("herdfs/mounts"))
}

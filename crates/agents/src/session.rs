//! One session directory: its files, and running the agent command for it.
//!
//! Session files come from whoever can write the host's directory, so they are opened only as
//! regular files and never through a symlink, which could point at the runner host's own files.

use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, bail, ensure};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const OUTSIDE_MOUNT: &str = "cwd outside mount";

/// A prompt's size and mtime. Unchanged across two polls means its writer is done.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Stamp {
    len: u64,
    modified: SystemTime,
}

/// Runs the agent command for sessions, inside the mount.
pub(crate) struct Agent {
    /// Canonical, so resolved paths can be checked against it.
    pub(crate) mount: PathBuf,
    pub(crate) command: String,
}

impl Agent {
    /// Runs the session in `dir` and records `done` or `failed: <reason>` as its status.
    pub(crate) async fn run(&self, dir: &Path) -> io::Result<()> {
        let status = match self.execute(dir).await {
            Ok(()) => "done".to_owned(),
            Err(err) => format!("failed: {err:#}"),
        };
        set_status(dir, &status).await
    }

    async fn execute(&self, dir: &Path) -> anyhow::Result<()> {
        let cwd = self.cwd(dir).await?;
        let prompt = open(&dir.join("prompt"), OpenOptions::new().read(true)).await?;
        let output = open(&dir.join("output"), OpenOptions::new().create(true).append(true)).await?;
        let stdout = output.try_clone().await?.into_std().await;
        let exit = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .current_dir(cwd)
            .env("HERDFS_SESSION_DIR", dir)
            .env("HERDFS_MOUNT", &self.mount)
            .stdin(prompt.into_std().await)
            .stdout(stdout.try_clone()?)
            .stderr(stdout)
            .status()
            .await
            .context("spawn")?;
        // On herdfs, fsync means committed: other hosts must never see the session end
        // before they can see all of its output.
        output.sync_all().await.context("sync output")?;
        let Some(code) = exit.code() else { bail!("{exit}") };
        ensure!(code == 0, "exit {code}");
        Ok(())
    }

    /// The directory named by the session's `cwd` file, or the mount root without one.
    async fn cwd(&self, dir: &Path) -> anyhow::Result<PathBuf> {
        let Some(text) = read(&dir.join("cwd")).await? else {
            return Ok(self.mount.clone());
        };
        let relative = Path::new(text.trim());
        let plain = relative
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
        ensure!(plain, OUTSIDE_MOUNT);
        let resolved = fs::canonicalize(self.mount.join(relative)).await.context("cwd")?;
        ensure!(resolved.starts_with(&self.mount), OUTSIDE_MOUNT);
        Ok(resolved)
    }
}

/// The prompt's stamp, if the session has a non-empty prompt and no status yet.
pub(crate) async fn new_prompt(dir: &Path) -> Option<Stamp> {
    // A status of any kind, even one we cannot check, counts as present: never starting beats
    // starting twice.
    let status = fs::symlink_metadata(dir.join("status")).await;
    if !matches!(status, Err(err) if err.kind() == ErrorKind::NotFound) {
        return None;
    }
    let meta = fs::symlink_metadata(dir.join("prompt")).await.ok()?;
    // Another host's create shows an empty prompt before the writer's close commits its bytes.
    if meta.len() == 0 {
        return None;
    }
    Some(Stamp {
        len: meta.len(),
        modified: meta.modified().ok()?,
    })
}

/// Marks the session `running`, once its directory proves to be inside the host's.
pub(crate) async fn claim(dir: &Path) -> io::Result<()> {
    check_dir(dir).await?;
    set_status(dir, "running").await
}

/// Marks the session failed if a previous runner left it `running`.
pub(crate) async fn recover(dir: &Path) -> io::Result<()> {
    check_dir(dir).await?;
    if read(&dir.join("status"))
        .await?
        .is_some_and(|status| status.trim() == "running")
    {
        set_status(dir, "failed: runner restarted").await?;
    }
    Ok(())
}

/// Fails if `dir` is reached through a symlink. It is named under the canonical mount, so
/// resolving to itself keeps it inside `<mount>/.agents/<host>/`.
async fn check_dir(dir: &Path) -> io::Result<()> {
    if fs::canonicalize(dir).await? == dir {
        return Ok(());
    }
    Err(io::Error::other("session directory is reached through a symlink"))
}

async fn set_status(dir: &Path, status: &str) -> io::Result<()> {
    let mut file = open(
        &dir.join("status"),
        OpenOptions::new().write(true).create(true).truncate(true),
    )
    .await?;
    file.write_all(format!("{status}\n").as_bytes()).await?;
    file.sync_all().await
}

/// A session file's text, or `None` if it does not exist.
async fn read(path: &Path) -> io::Result<Option<String>> {
    let mut file = match open(path, OpenOptions::new().read(true)).await {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut text = String::new();
    file.read_to_string(&mut text).await?;
    Ok(Some(text))
}

/// Opens a session file only if it is a regular file, without following a symlink.
async fn open(path: &Path, options: &mut OpenOptions) -> io::Result<File> {
    // O_NONBLOCK refuses a FIFO below instead of hanging on it; regular files ignore the flag.
    let opened = options
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .await;
    let not_regular = || io::Error::other("not a regular file in the session");
    let file = match opened {
        Err(err) if matches!(err.raw_os_error(), Some(libc::ELOOP | libc::EISDIR | libc::ENXIO)) => {
            return Err(not_regular());
        }
        opened => opened?,
    };
    if !file.metadata().await?.is_file() {
        return Err(not_regular());
    }
    Ok(file)
}

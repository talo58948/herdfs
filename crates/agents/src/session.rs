//! One session directory: its files, and running the agent command for it.

use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, bail, ensure};
use tokio::fs;
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
        let prompt = fs::File::open(dir.join("prompt")).await.context("prompt")?;
        let output = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("output"))
            .await
            .context("output")?
            .into_std()
            .await;
        let status = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .current_dir(cwd)
            .env("HERDFS_SESSION_DIR", dir)
            .env("HERDFS_MOUNT", &self.mount)
            .stdin(prompt.into_std().await)
            .stdout(output.try_clone()?)
            .stderr(output)
            .status()
            .await
            .context("spawn")?;
        let Some(code) = status.code() else {
            bail!("{status}")
        };
        ensure!(code == 0, "exit {code}");
        Ok(())
    }

    /// The directory named by the session's `cwd` file, or the mount root without one.
    async fn cwd(&self, dir: &Path) -> anyhow::Result<PathBuf> {
        let relative = match fs::read_to_string(dir.join("cwd")).await {
            Ok(text) => PathBuf::from(text.trim()),
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(self.mount.clone()),
            Err(err) => return Err(err).context("cwd"),
        };
        let plain = relative
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
        ensure!(plain, OUTSIDE_MOUNT);
        let resolved = fs::canonicalize(self.mount.join(relative)).await.context("cwd")?;
        ensure!(resolved.starts_with(&self.mount), OUTSIDE_MOUNT);
        Ok(resolved)
    }
}

/// The prompt's stamp, if the session has a prompt and no status yet.
pub(crate) async fn new_prompt(dir: &Path) -> Option<Stamp> {
    // A status we cannot check counts as present: never starting beats starting twice.
    if fs::try_exists(dir.join("status")).await.unwrap_or(true) {
        return None;
    }
    let meta = fs::metadata(dir.join("prompt")).await.ok()?;
    Some(Stamp {
        len: meta.len(),
        modified: meta.modified().ok()?,
    })
}

/// Marks the session failed if a previous runner left it `running`.
pub(crate) async fn recover(dir: &Path) -> io::Result<()> {
    match fs::read_to_string(dir.join("status")).await {
        Ok(status) if status.trim() == "running" => set_status(dir, "failed: runner restarted").await,
        Err(err) if err.kind() != ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

pub(crate) async fn set_status(dir: &Path, status: &str) -> io::Result<()> {
    fs::write(dir.join("status"), format!("{status}\n")).await
}

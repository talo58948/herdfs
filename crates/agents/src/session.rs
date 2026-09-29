//! One session directory: its files, and running the agent command for it.
//!
//! Session files come from whoever can write the host's directory, so they are opened only as
//! regular files and never through a symlink, which could point at the runner host's own files.

use std::ffi::{CString, OsStr};
use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
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

/// A claimed session, held by its directory rather than its replaceable path.
pub(crate) struct Session {
    dir: PathBuf,
    file: std::fs::File,
}

impl Agent {
    /// Runs the session and records `done` or `failed: <reason>` as its status.
    pub(crate) async fn run(&self, session: &Session) -> io::Result<()> {
        let status = match self.execute(session).await {
            Ok(()) => "done".to_owned(),
            Err(err) => format!("failed: {err:#}"),
        };
        set_status(session, &status).await
    }

    async fn execute(&self, session: &Session) -> anyhow::Result<()> {
        let cwd = self.cwd(session).await?;
        let prompt = open(session, "prompt", libc::O_RDONLY).await?;
        let output = open(session, "output", libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND).await?;
        let stdout = output.try_clone().await?.into_std().await;
        let exit = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .current_dir(cwd)
            .env("HERDFS_SESSION_DIR", &session.dir)
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
    async fn cwd(&self, session: &Session) -> anyhow::Result<PathBuf> {
        let Some(text) = read(session, "cwd").await? else {
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
pub(crate) async fn claim(dir: &Path) -> io::Result<Session> {
    check_dir(dir).await?;
    let session = open_session(dir).await?;
    set_status(&session, "running").await?;
    Ok(session)
}

/// Marks the session failed if a previous runner left it `running`.
pub(crate) async fn recover(dir: &Path) -> io::Result<()> {
    check_dir(dir).await?;
    let session = open_session(dir).await?;
    if read(&session, "status")
        .await?
        .is_some_and(|status| status.trim() == "running")
    {
        set_status(&session, "failed: runner restarted").await?;
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

async fn set_status(session: &Session, status: &str) -> io::Result<()> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
    let mut file = open(session, "status", flags).await?;
    file.write_all(format!("{status}\n").as_bytes()).await?;
    file.sync_all().await
}

/// A session file's text, or `None` if it does not exist.
async fn read(session: &Session, name: &str) -> io::Result<Option<String>> {
    let mut file = match open(session, name, libc::O_RDONLY).await {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut text = String::new();
    file.read_to_string(&mut text).await?;
    Ok(Some(text))
}

/// Opens a session file only if it is a regular file, without following a symlink.
async fn open(session: &Session, name: &str, flags: libc::c_int) -> io::Result<File> {
    let opened = open_at(
        &session.file,
        OsStr::new(name),
        flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0o666,
    )
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

async fn open_session(dir: &Path) -> io::Result<Session> {
    let parent = dir
        .parent()
        .ok_or_else(|| io::Error::other("session has no host directory"))?;
    let name = dir
        .file_name()
        .ok_or_else(|| io::Error::other("session has no name"))?;
    let host = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)
        .await?
        .into_std()
        .await;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let file = open_at(&host, name, flags, 0).await?;
    Ok(Session {
        dir: dir.to_owned(),
        file: file.into_std().await,
    })
}

async fn open_at(
    dir: &std::fs::File,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    let dir = dir.try_clone()?;
    let name = name.to_owned();
    let file = tokio::task::spawn_blocking(move || open_at_sync(&dir, &name, flags, mode))
        .await
        .map_err(io::Error::other)??;
    Ok(File::from_std(file))
}

#[expect(unsafe_code, reason = "libc has no safe openat wrapper")]
fn open_at_sync(
    dir: &std::fs::File,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<std::fs::File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "path component contains NUL"))?;
    let descriptor = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, mode) };
    if descriptor == -1 {
        return Err(io::Error::last_os_error());
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    Ok(descriptor.into())
}

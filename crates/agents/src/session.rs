//! One session directory: its files, and running the agent command for it.
//!
//! Session files come from whoever can write the host's directory, so they are opened only as
//! regular files and never through a symlink, which could point at the runner host's own files.

mod process;

use std::ffi::OsStr;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, bail};
use rustix::fs::{Mode, OFlags};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use process::Outcome;

const OUTSIDE_MOUNT: &str = "cwd outside mount";
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE_MODE: Mode = Mode::from_raw_mode(0o666);

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
    /// Runs the session and records its terminal status.
    pub(crate) async fn run(&self, session: &Session) -> io::Result<()> {
        let status = match self.execute(session).await {
            Ok(Outcome::Cancelled) => "cancelled".to_owned(),
            Ok(Outcome::Done) => "done".to_owned(),
            Err(err) => format!("failed: {err:#}"),
        };
        transition(session, Some("running"), &status).await
    }

    /// The directory named by the session's `cwd` file, or the mount root without one.
    async fn cwd(&self, session: &Session) -> anyhow::Result<std::fs::File> {
        let Some(text) = read(session, "cwd").await? else {
            return open_below(&self.mount, Path::new("")).await.context("cwd");
        };
        let relative = Path::new(text.trim());
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            bail!(OUTSIDE_MOUNT);
        }
        match open_below(&self.mount, relative).await {
            Err(err) if matches!(err.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                bail!(OUTSIDE_MOUNT)
            }
            opened => opened.context("cwd"),
        }
    }
}

/// The prompt's stamp and whether the session is already queued.
pub(crate) async fn pending_prompt(mount: &Path, dir: &Path) -> io::Result<Option<(Stamp, bool)>> {
    let session = session(mount, dir).await?;
    let queued = match read(&session, "status").await? {
        None => false,
        Some(status) if status.trim() == "queued" => true,
        Some(_) => return Ok(None),
    };
    let meta = match fs::symlink_metadata(dir.join("prompt")).await {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    // Another host's create shows an empty prompt before the writer's close commits its bytes.
    if meta.len() == 0 {
        return Ok(None);
    }
    let modified = meta.modified()?;
    Ok(Some((
        Stamp {
            len: meta.len(),
            modified,
        },
        queued,
    )))
}

/// Marks a newly discovered session `queued`.
pub(crate) async fn queue(mount: &Path, dir: &Path) -> io::Result<()> {
    let session = session(mount, dir).await?;
    transition(&session, None, "queued").await
}

/// Cancels a queued session without waiting for a runner slot.
pub(crate) async fn cancel_queued(mount: &Path, dir: &Path) -> io::Result<bool> {
    let session = session(mount, dir).await?;
    match open(&session, "cancel", OFlags::RDONLY).await {
        Ok(_) => {
            transition(&session, Some("queued"), "cancelled").await?;
            Ok(true)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Moves a queued session to `running`, once its directory proves to be inside the host's.
pub(crate) async fn claim(mount: &Path, dir: &Path) -> io::Result<Session> {
    let session = session(mount, dir).await?;
    transition(&session, Some("queued"), "running").await?;
    Ok(session)
}

/// Marks the session failed if a previous runner left it `running`.
pub(crate) async fn recover(mount: &Path, dir: &Path) -> io::Result<()> {
    let session = session(mount, dir).await?;
    if read(&session, "status")
        .await?
        .is_some_and(|status| status.trim() == "running")
    {
        transition(&session, Some("running"), "failed: runner restarted").await?;
    }
    Ok(())
}

async fn transition(session: &Session, from: Option<&str>, status: &str) -> io::Result<()> {
    let current = read(session, "status").await?;
    if current.as_deref().map(str::trim) != from {
        return Err(io::Error::other(format!(
            "status changed before transition to {status}"
        )));
    }
    write(session, "status", status).await
}

async fn write(session: &Session, name: &str, text: &str) -> io::Result<()> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC;
    let mut file = open(session, name, flags).await?;
    file.write_all(format!("{text}\n").as_bytes()).await?;
    file.sync_all().await
}

/// A session file's text, or `None` if it does not exist.
async fn read(session: &Session, name: &str) -> io::Result<Option<String>> {
    let mut file = match open(session, name, OFlags::RDONLY).await {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut text = String::new();
    file.read_to_string(&mut text).await?;
    Ok(Some(text))
}

/// Opens a session file only if it is a regular file, without following a symlink.
async fn open(session: &Session, name: &str, flags: OFlags) -> io::Result<File> {
    let opened = open_at(
        &session.file,
        OsStr::new(name),
        flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        FILE_MODE,
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

async fn session(mount: &Path, dir: &Path) -> io::Result<Session> {
    // Walked from the canonical mount, so no component may be a symlink.
    let relative = dir
        .strip_prefix(mount)
        .map_err(|_| io::Error::other("session is not below its mount"))?;
    let file = open_below(mount, relative).await?;
    Ok(Session {
        dir: dir.to_owned(),
        file,
    })
}

async fn open_at(dir: &std::fs::File, name: &OsStr, flags: OFlags, mode: Mode) -> io::Result<File> {
    let dir = dir.try_clone()?;
    let name = name.to_owned();
    let file =
        tokio::task::spawn_blocking(move || rustix::fs::openat(&dir, &name, flags, mode).map(Into::into))
            .await
            .map_err(io::Error::other)??;
    Ok(File::from_std(file))
}

async fn open_below(root: &Path, relative: &Path) -> io::Result<std::fs::File> {
    let (root, relative) = (root.to_owned(), relative.to_owned());
    tokio::task::spawn_blocking(move || {
        let mut dir: std::fs::File = rustix::fs::open(&root, DIR_FLAGS, Mode::empty())?.into();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                if component == Component::CurDir {
                    continue;
                }
                return Err(io::Error::other(OUTSIDE_MOUNT));
            };
            dir = rustix::fs::openat(&dir, name, DIR_FLAGS, Mode::empty())?.into();
        }
        Ok(dir)
    })
    .await
    .map_err(io::Error::other)?
}

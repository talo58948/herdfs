use std::ffi::OsStr;
use std::io;

use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags, flock};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use super::{DIR_FLAGS, FILE_MODE, Session, open_at};

/// A uniquely named file beside its target, removed unless renamed into place.
struct Scratch {
    dir: std::fs::File,
    name: String,
    file: File,
    renamed: bool,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.renamed {
            let _ = rustix::fs::unlinkat(&self.dir, &self.name, AtFlags::empty());
        }
    }
}

pub(super) async fn lock_transition(session: &Session) -> io::Result<std::fs::File> {
    let anchor = session.file.try_clone()?;
    tokio::task::spawn_blocking(move || {
        // A fresh open description releases the flock when the returned guard drops.
        let dir: std::fs::File = rustix::fs::openat(&anchor, ".", DIR_FLAGS, Mode::empty())?.into();
        flock(&dir, FlockOperation::LockExclusive).map_err(io::Error::from)?;
        Ok(dir)
    })
    .await
    .map_err(io::Error::other)?
}

pub(super) async fn write_atomic(session: &Session, name: &str, contents: &[u8]) -> io::Result<()> {
    let mut scratch = create(session, name).await?;
    scratch.file.write_all(contents).await?;
    scratch.file.sync_all().await?;
    let dir = scratch.dir.try_clone()?;
    let temporary = scratch.name.clone();
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || rustix::fs::renameat(&dir, temporary, &dir, name))
        .await
        .map_err(io::Error::other)??;
    scratch.renamed = true;
    Ok(())
}

async fn create(session: &Session, name: &str) -> io::Result<Scratch> {
    // The session directory is caller-writable, so the scratch name must be unpredictable.
    let temporary = format!(".{name}.{:032x}.tmp", rand::random::<u128>());
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let file = open_at(
        &session.file,
        OsStr::new(&temporary),
        flags | OFlags::CLOEXEC,
        FILE_MODE,
    )
    .await?;
    Ok(Scratch {
        dir: session.file.try_clone()?,
        name: temporary,
        file,
        renamed: false,
    })
}

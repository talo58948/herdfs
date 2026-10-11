use std::ffi::OsStr;
use std::io::{self, ErrorKind};

use rustix::fs::{AtFlags, OFlags};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use super::{FILE_MODE, Session, open_at};

const CREATE_ATTEMPTS: usize = 8;

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
    let dir = session.file.try_clone()?;
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL;
    for _ in 0..CREATE_ATTEMPTS {
        // The session directory is caller-writable, so scratch names must be unpredictable.
        let temporary = format!(".{name}.{:032x}.tmp", rand::random::<u128>());
        let opened = open_at(
            &session.file,
            OsStr::new(&temporary),
            flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            FILE_MODE,
        )
        .await;
        match opened {
            Ok(file) => {
                return Ok(Scratch {
                    dir,
                    name: temporary,
                    file,
                    renamed: false,
                });
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        ErrorKind::AlreadyExists,
        "could not create a unique session scratch file",
    ))
}

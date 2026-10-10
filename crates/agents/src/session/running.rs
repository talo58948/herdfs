//! Host-local identity for process groups that outlive a crashed runner.

mod processes;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::Session;
use super::process::kill_group;
use processes::{boot_id, start_time};

pub(super) struct Running {
    path: PathBuf,
    group: u32,
    started: String,
    boot: String,
    armed: bool,
}

impl Running {
    pub(super) fn record(session: &Session, group: u32) -> io::Result<Self> {
        let running = Self {
            path: state_path(session)?,
            group,
            started: start_time(group)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "process group leader exited"))?,
            boot: boot_id()?,
            armed: true,
        };
        let temporary = running.path.with_extension(format!("{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        write!(file, "{}\n{}\n{}\n", running.group, running.started, running.boot)?;
        file.sync_all()?;
        fs::rename(temporary, &running.path)?;
        Ok(running)
    }

    pub(super) fn finish(mut self) -> io::Result<()> {
        remove(&self.path)?;
        self.armed = false;
        Ok(())
    }

    fn load(session: &Session) -> io::Result<Option<Self>> {
        let path = state_path(session)?;
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let mut lines = text.lines();
        let group = lines
            .next()
            .and_then(|line| line.parse().ok())
            .ok_or_else(|| invalid("process group"))?;
        let started = lines
            .next()
            .filter(|line| !line.is_empty())
            .ok_or_else(|| invalid("process start time"))?
            .to_owned();
        let boot = lines
            .next()
            .filter(|line| !line.is_empty())
            .ok_or_else(|| invalid("boot id"))?
            .to_owned();
        Ok(Some(Self {
            path,
            group,
            started,
            boot,
            armed: true,
        }))
    }

    fn stop(&mut self) -> io::Result<()> {
        if boot_id()? == self.boot {
            match start_time(self.group)? {
                Some(started) if started != self.started => {}
                Some(_) | None => kill_group(self.group)?,
            }
        }
        remove(&self.path)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.stop();
        }
    }
}

pub(super) fn stop_previous(session: &Session) -> io::Result<()> {
    if let Some(mut running) = Running::load(session)? {
        running.stop()?;
    }
    Ok(())
}

fn state_path(session: &Session) -> io::Result<PathBuf> {
    let metadata = session.file.metadata()?;
    let root = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
        || std::env::temp_dir().join(format!("herdfs-agents-{}", metadata.uid())),
        |dir| PathBuf::from(dir).join("herdfs-agents"),
    );
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    Ok(root.join(format!("{}-{}", metadata.dev(), metadata.ino())))
}

fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn invalid(field: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("invalid {field} record"))
}

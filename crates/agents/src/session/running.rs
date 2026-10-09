//! Host-local identity for process groups that outlive a crashed runner.

use std::fs::{self, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::Session;
use super::process::kill_group;

pub(super) struct Running {
    path: PathBuf,
    group: u32,
    started: String,
    armed: bool,
}

impl Running {
    pub(super) fn record(session: &Session, group: u32) -> io::Result<Self> {
        let running = Self {
            path: state_path(session)?,
            group,
            started: start_time(group)?,
            armed: true,
        };
        let temporary = running.path.with_extension(format!("{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        write!(file, "{}\n{}\n", running.group, running.started)?;
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
        Ok(Some(Self {
            path,
            group,
            started,
            armed: true,
        }))
    }

    fn stop(&mut self) -> io::Result<()> {
        match start_time(self.group) {
            Ok(started) if started == self.started => kill_group(self.group)?,
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
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
    let mut path_hash = DefaultHasher::new();
    session.dir.hash(&mut path_hash);
    let root = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
        || std::env::temp_dir().join(format!("herdfs-agents-{}", metadata.uid())),
        |dir| PathBuf::from(dir).join("herdfs-agents"),
    );
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    Ok(root.join(format!(
        "{:016x}-{}-{}",
        path_hash.finish(),
        metadata.dev(),
        metadata.ino()
    )))
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

#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> io::Result<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .ok_or_else(|| invalid("/proc process stat"))?;
    let fields = fields.split_whitespace();
    let group = fields
        .clone()
        .nth(2)
        .ok_or_else(|| invalid("/proc process group"))?;
    let started = fields
        .clone()
        .nth(19)
        .ok_or_else(|| invalid("/proc process start time"))?;
    Ok(format!("{started}:{group}"))
}

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "proc_pidinfo provides precise process identity on macOS"
)]
fn start_time(pid: u32) -> io::Result<String> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: proc_pidinfo receives a correctly sized, writable PROC_PIDTBSDINFO buffer.
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a full buffer was initialized by proc_pidinfo.
    let info = unsafe { info.assume_init() };
    Ok(format!(
        "{}:{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec, info.pbi_pgid
    ))
}

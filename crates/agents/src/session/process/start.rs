//! Process startup gated on a durable recovery record.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Mutex;

use anyhow::{Context, anyhow};
use tokio::process::{Child, Command};

use super::kill_group;
use crate::session::{Session, running::Running};

static START: Mutex<()> = Mutex::new(());

pub(super) fn spawn(
    mut command: Command,
    session: &Session,
    cwd: &File,
) -> anyhow::Result<(Child, anyhow::Result<Running>)> {
    // A second fork could inherit a first child's release end while both wait before exec.
    let _start = START.lock().map_err(|_| anyhow!("process start lock poisoned"))?;
    let (mut ready, child_ready) = start_pipe().context("create ready pipe")?;
    let (child_gate, mut release) = start_pipe().context("create start gate")?;
    child_context(
        &mut command,
        cwd.as_raw_fd(),
        ready.as_raw_fd(),
        release.as_raw_fd(),
        child_ready.as_raw_fd(),
        child_gate.as_raw_fd(),
    );
    let runtime = tokio::runtime::Handle::current();

    std::thread::scope(|scope| {
        let spawning = scope.spawn(move || {
            let _runtime = runtime.enter();
            let spawned = command.spawn();
            drop((child_ready, child_gate));
            spawned
        });
        let running = read_group(&mut ready).and_then(|group| {
            let running = Running::record(session, group).context("record process group");
            match running {
                Ok(running) => release
                    .write_all(&[1])
                    .context("release process group")
                    .map(|()| running),
                Err(err) => {
                    kill_group(group).context("kill after process-group record failure")?;
                    Err(err)
                }
            }
        });
        drop(release);
        let child = spawning
            .join()
            .map_err(|_| anyhow!("spawn thread panicked"))?
            .context("spawn")?;
        Ok((child, running))
    })
}

fn read_group(ready: &mut File) -> anyhow::Result<u32> {
    let mut bytes = [0; size_of::<libc::pid_t>()];
    ready.read_exact(&mut bytes).context("read process group")?;
    u32::try_from(libc::pid_t::from_ne_bytes(bytes)).context("invalid process group")
}

fn start_pipe() -> io::Result<(File, File)> {
    let (read, write) = rustix::pipe::pipe()?;
    set_cloexec(&read)?;
    set_cloexec(&write)?;
    Ok((read.into(), write.into()))
}

#[expect(unsafe_code, reason = "fcntl sets close-on-exec on the new pipe")]
fn set_cloexec(fd: &OwnedFd) -> io::Result<()> {
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[expect(
    unsafe_code,
    reason = "the checked cwd and process gate must be installed between fork and exec"
)]
fn child_context(
    command: &mut Command,
    cwd: libc::c_int,
    parent_ready: libc::c_int,
    parent_release: libc::c_int,
    ready: libc::c_int,
    gate: libc::c_int,
) {
    unsafe {
        command.pre_exec(move || {
            libc::close(parent_ready);
            libc::close(parent_release);
            if libc::fchdir(cwd) == -1 || libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            write_pid(ready)?;
            wait_for_release(gate)
        });
    }
}

#[expect(unsafe_code, reason = "only async-signal-safe calls are valid after fork")]
fn write_pid(fd: libc::c_int) -> io::Result<()> {
    let bytes = unsafe { libc::getpid() }.to_ne_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        let written = unsafe { libc::write(fd, bytes[offset..].as_ptr().cast(), bytes.len() - offset) };
        if written > 0 {
            offset += written as usize;
            continue;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    Ok(())
}

#[expect(unsafe_code, reason = "only async-signal-safe calls are valid after fork")]
fn wait_for_release(fd: libc::c_int) -> io::Result<()> {
    let mut byte = 0_u8;
    loop {
        let read = unsafe { libc::read(fd, (&raw mut byte).cast(), 1) };
        if read == 1 {
            return Ok(());
        }
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

//! Child-process setup, output capture and cancellation for a session.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use rustix::fs::OFlags;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::task::JoinSet;

use super::running::{self, Running};
use super::{Agent, Session, open, write};

const CANCEL_POLL: Duration = Duration::from_millis(100);
const CAPTURE_DRAIN: Duration = Duration::from_secs(1);

pub(super) enum Outcome {
    Done,
    Cancelled,
}

impl Agent {
    pub(super) async fn execute(&self, session: &Session) -> anyhow::Result<Outcome> {
        let cwd = self.cwd(session).await?;
        let prompt = open(session, "prompt", OFlags::RDONLY).await?;
        let output = result_file(session, "output").await?;
        let stdout = result_file(session, "stdout").await?;
        let stderr = result_file(session, "stderr").await?;
        let stdout_sync = stdout.try_clone().await?;
        let stderr_sync = stderr.try_clone().await?;
        let session_id = running::session_id();
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&self.command)
            .env("HERDFS_SESSION_DIR", &session.dir)
            .env("HERDFS_SESSION_ID", &session_id)
            .env("HERDFS_MOUNT", &self.mount)
            .stdin(prompt.into_std().await)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        child_context(&mut command, cwd.as_raw_fd());
        let mut child = command.spawn().context("spawn")?;
        let group = child.id().context("child has no pid")?;
        let running = match Running::record(session, group, session_id) {
            Ok(running) => running,
            Err(err) => {
                kill_group(group).context("kill after process-group record failure")?;
                child
                    .wait()
                    .await
                    .context("wait after process-group record failure")?;
                return Err(err).context("record process group");
            }
        };
        drop(cwd);

        let child_stdout = child.stdout.take().context("stdout pipe")?;
        let child_stderr = child.stderr.take().context("stderr pipe")?;
        let mut captures = JoinSet::new();
        captures.spawn(capture(child_stdout, stdout, output.try_clone().await?));
        captures.spawn(capture(child_stderr, stderr, output.try_clone().await?));
        let (exit, cancelled) = wait(session, &mut child, group).await?;
        running.finish()?;
        let capture_result = finish_captures(&mut captures).await;
        // On herdfs, fsync means committed: terminal status must not precede its results.
        stdout_sync.sync_all().await.context("sync stdout")?;
        stderr_sync.sync_all().await.context("sync stderr")?;
        output.sync_all().await.context("sync output")?;
        capture_result?;
        write(session, "exit_code", &exit_code(exit)).await?;

        if cancelled {
            return Ok(Outcome::Cancelled);
        }
        let Some(code) = exit.code() else { bail!("{exit}") };
        ensure!(code == 0, "exit {code}");
        Ok(Outcome::Done)
    }
}

async fn finish_captures(captures: &mut JoinSet<io::Result<()>>) -> anyhow::Result<()> {
    let finished = tokio::time::timeout(CAPTURE_DRAIN, async {
        while let Some(result) = captures.join_next().await {
            result.context("capture task")??;
        }
        Ok(())
    })
    .await;
    let result = match finished {
        Ok(result) => result,
        Err(_) => Ok(()),
    };
    captures.abort_all();
    while captures.join_next().await.is_some() {}
    result
}

async fn result_file(session: &Session, name: &str) -> io::Result<File> {
    open(session, name, OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC).await
}

async fn capture(
    mut source: impl AsyncRead + Unpin,
    mut separate: File,
    mut combined: File,
) -> io::Result<()> {
    let mut buffer = [0; 8192];
    let mut first_error = None;
    loop {
        let read = source.read(&mut buffer).await?;
        if read == 0 {
            if let Some(err) = first_error {
                return Err(err);
            }
            return separate.sync_all().await;
        }
        if first_error.is_some() {
            // Draining keeps the child from blocking on a full pipe.
            continue;
        }
        if let Err(err) = separate.write_all(&buffer[..read]).await {
            first_error = Some(err);
        } else if let Err(err) = combined.write_all(&buffer[..read]).await {
            first_error = Some(err);
        }
    }
}

async fn wait(session: &Session, child: &mut Child, group: u32) -> anyhow::Result<(ExitStatus, bool)> {
    tokio::select! {
        exit = child.wait() => {
            let exit = exit.context("wait")?;
            kill_group(group)?;
            Ok((exit, false))
        },
        cancelled = cancellation(session) => {
            kill_group(group)?;
            let exit = child.wait().await.context("wait after cancel")?;
            cancelled.context("watch cancel")?;
            Ok((exit, true))
        }
    }
}

async fn cancellation(session: &Session) -> io::Result<()> {
    loop {
        match open(session, "cancel", OFlags::RDONLY).await {
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

fn exit_code(exit: ExitStatus) -> String {
    exit.code().map_or_else(
        || format!("signal {}", exit.signal().unwrap_or_default()),
        |code| code.to_string(),
    )
}

#[expect(
    unsafe_code,
    reason = "fchdir must run between fork and exec to hold the checked cwd"
)]
fn child_context(command: &mut Command, cwd: RawFd) {
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(cwd) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub(super) fn kill_group(group: u32) -> io::Result<()> {
    kill(-(group as libc::pid_t))
}

pub(super) fn kill_process(pid: u32) -> io::Result<()> {
    kill(pid as libc::pid_t)
}

#[expect(unsafe_code, reason = "libc is the portable Unix process signalling API")]
fn kill(target: libc::pid_t) -> io::Result<()> {
    let result = unsafe { libc::kill(target, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}

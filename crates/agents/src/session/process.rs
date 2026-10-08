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

use super::{Agent, Session, open, write};

const CANCEL_POLL: Duration = Duration::from_millis(100);

pub(super) enum Outcome {
    Done,
    Cancelled,
}

impl Agent {
    pub(super) async fn execute(&self, session: &Session) -> anyhow::Result<Outcome> {
        let cwd = self.cwd(session).await?;
        let prompt = open(session, "prompt", OFlags::RDONLY).await?;
        let output = open(
            session,
            "output",
            OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND,
        )
        .await?;
        let stdout = result_file(session, "stdout").await?;
        let stderr = result_file(session, "stderr").await?;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&self.command)
            .env("HERDFS_SESSION_DIR", &session.dir)
            .env("HERDFS_MOUNT", &self.mount)
            .stdin(prompt.into_std().await)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        child_context(&mut command, cwd.as_raw_fd());
        let mut child = command.spawn().context("spawn")?;
        drop(cwd);

        let child_stdout = child.stdout.take().context("stdout pipe")?;
        let child_stderr = child.stderr.take().context("stderr pipe")?;
        let stdout_copy = tokio::spawn(capture(child_stdout, stdout, output.try_clone().await?));
        let stderr_copy = tokio::spawn(capture(child_stderr, stderr, output.try_clone().await?));
        let (exit, cancelled) = wait(session, &mut child).await?;
        let (stdout_result, stderr_result) = tokio::join!(stdout_copy, stderr_copy);
        stdout_result.context("capture stdout task")??;
        stderr_result.context("capture stderr task")??;
        // On herdfs, fsync means committed: terminal status must not precede its results.
        output.sync_all().await.context("sync output")?;
        write(session, "exit_code", &exit_code(exit)).await?;

        if cancelled {
            return Ok(Outcome::Cancelled);
        }
        let Some(code) = exit.code() else { bail!("{exit}") };
        ensure!(code == 0, "exit {code}");
        Ok(Outcome::Done)
    }
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
    let mut separate_error = None;
    let mut combined_error = None;
    loop {
        let read = source.read(&mut buffer).await?;
        if read == 0 {
            if separate_error.is_none()
                && let Err(err) = separate.sync_all().await
            {
                separate_error = Some(err);
            }
            return separate_error.or(combined_error).map_or(Ok(()), Err);
        }
        if separate_error.is_none()
            && let Err(err) = separate.write_all(&buffer[..read]).await
        {
            separate_error = Some(err);
        }
        if combined_error.is_none()
            && let Err(err) = combined.write_all(&buffer[..read]).await
        {
            combined_error = Some(err);
        }
    }
}

async fn wait(session: &Session, child: &mut Child) -> anyhow::Result<(ExitStatus, bool)> {
    tokio::select! {
        exit = child.wait() => Ok((exit.context("wait")?, false)),
        cancelled = cancellation(session) => {
            if let Err(err) = cancelled {
                kill_group(child)?;
                child.wait().await.context("wait after cancel error")?;
                return Err(err).context("watch cancel");
            }
            kill_group(child)?;
            Ok((child.wait().await.context("wait after cancel")?, true))
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

#[expect(
    unsafe_code,
    reason = "libc is the portable Unix process-group signalling API"
)]
fn kill_group(child: &Child) -> io::Result<()> {
    let pid = child.id().ok_or_else(|| io::Error::other("child has no pid"))?;
    let result = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}

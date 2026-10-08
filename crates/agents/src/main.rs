//! Runs agent sessions that users request by creating files under `<mount>/.agents/<host>/`.
//!
//! A session is a directory there with a `prompt` and optionally a `cwd`. The prompt must be
//! non-empty: another host's newly created file reads empty until its writer's close commits it.

mod session;

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::fs;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use session::{Agent, Stamp};

#[derive(Parser)]
struct Args {
    /// Root of the herdfs mount; any directory works.
    #[arg(long)]
    mount: PathBuf,
    /// Serves the sessions under `<mount>/.agents/<host>/`.
    #[arg(long, value_parser = host)]
    host: String,
    /// Shell command run for each session, with the prompt on stdin.
    #[arg(long)]
    command: String,
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..))]
    max_sessions: u32,
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..))]
    poll_ms: u64,
}

fn host(value: &str) -> Result<String, String> {
    if value.is_empty() || value == "." || value == ".." || value.contains('/') {
        return Err("host must be one plain path component".into());
    }
    Ok(value.into())
}

/// Starts this host's sessions once their prompts are complete, as slots free up.
struct Runner {
    host_dir: PathBuf,
    agent: Arc<Agent>,
    slots: Arc<Semaphore>,
    /// Prompt stamps seen by the previous poll, for sessions not started yet.
    seen: HashMap<PathBuf, Stamp>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mount = fs::canonicalize(&args.mount)
        .await
        .with_context(|| format!("mount {}", args.mount.display()))?;
    let mut runner = Runner {
        host_dir: mount.join(".agents").join(&args.host),
        agent: Arc::new(Agent {
            mount,
            command: args.command,
        }),
        slots: Arc::new(Semaphore::new(args.max_sessions as usize)),
        seen: HashMap::new(),
    };
    runner.recover().await?;
    let interval = Duration::from_millis(args.poll_ms);
    loop {
        tokio::select! {
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            () = tokio::time::sleep(interval) => runner.poll().await,
        }
    }
    // All slots are free only once every running session has finished.
    let _slots = runner.slots.acquire_many(args.max_sessions).await?;
    Ok(())
}

impl Runner {
    /// Marks sessions that a previous runner left `running` as failed.
    async fn recover(&self) -> io::Result<()> {
        for dir in session_dirs(&self.host_dir).await? {
            if let Err(err) = session::recover(&self.agent.mount, &dir).await {
                log_error(&dir, &err);
            }
        }
        Ok(())
    }

    /// Queues complete prompts, then starts queued sessions while slots are free.
    async fn poll(&mut self) {
        let dirs = match session_dirs(&self.host_dir).await {
            Ok(dirs) => dirs,
            Err(err) => {
                log_error(&self.host_dir, &err);
                return;
            }
        };
        let mut pending = HashMap::new();
        for dir in dirs {
            let (stamp, queued) = match session::pending_prompt(&self.agent.mount, &dir).await {
                Ok(Some(pending)) => pending,
                Ok(None) => continue,
                Err(err) => {
                    log_error(&dir, &err);
                    continue;
                }
            };
            if !queued {
                if self.seen.get(&dir) != Some(&stamp) {
                    pending.insert(dir, stamp);
                    continue;
                }
                if let Err(err) = session::queue(&self.agent.mount, &dir).await {
                    log_error(&dir, &err);
                    continue;
                }
            }
            if let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() {
                self.start(dir, slot).await;
                continue;
            }
            pending.insert(dir, stamp);
        }
        self.seen = pending;
    }

    /// Marks the session running and runs it in the background, holding `slot` until it ends.
    async fn start(&self, dir: PathBuf, slot: OwnedSemaphorePermit) {
        let session = match session::claim(&self.agent.mount, &dir).await {
            Ok(session) => session,
            Err(err) => {
                log_error(&dir, &err);
                return;
            }
        };
        let agent = Arc::clone(&self.agent);
        tokio::spawn(async move {
            if let Err(err) = agent.run(&session).await {
                log_error(&dir, &err);
            }
            drop(slot);
        });
    }
}

/// The session directories under `host_dir`; none while it does not exist.
async fn session_dirs(host_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut entries = match fs::read_dir(host_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut dirs = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

fn log_error(path: &Path, err: &io::Error) {
    eprintln!("herdfs-agents: {}: {err}", path.display());
}

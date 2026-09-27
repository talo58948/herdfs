//! Child processes of the lab, the ports they listen on, and the workspace binaries they run.

use std::fs::{self, File};
use std::net::{Ipv4Addr, TcpListener};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, io, thread};

use anyhow::{Context, Result, bail, ensure};

pub(crate) const AUTHORITY: &str = "herdfs-authority";
pub(crate) const CLIENT: &str = "herdfs";
pub(crate) const AGENTS: &str = "herdfs-agents";

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
const LOG_TAIL_LINES: usize = 20;
/// Below Linux's ephemeral range, so outgoing connections never hold a port the lab is about to use.
const PORTS: Range<u16> = 20_000..32_768;

/// A child process that is killed with SIGKILL and reaped when dropped.
pub(crate) struct Proc {
    name: String,
    child: Child,
    log: PathBuf,
}

impl Proc {
    /// Starts `command` with its stdout and stderr appended to `log`.
    pub(crate) fn spawn(name: &str, mut command: Command, log: &Path) -> Result<Self> {
        let out = File::options().create(true).append(true).open(log)?;
        command.stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
        let child = command
            .spawn()
            .with_context(|| format!("starting {name} ({:?})", command.get_program()))?;
        Ok(Self {
            name: name.to_owned(),
            child,
            log: log.to_owned(),
        })
    }

    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }

    /// Polls `ready` until it holds. Fails with the end of the log if the process exits first
    /// or does not get ready in time.
    pub(crate) fn wait_until(&mut self, what: &str, mut ready: impl FnMut() -> bool) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        while !ready() {
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "{} exited ({status}) before it was {what}\n{}",
                    self.name,
                    self.log_tail()
                );
            }
            ensure!(
                Instant::now() < deadline,
                "{} not {what} after {READY_TIMEOUT:?}\n{}",
                self.name,
                self.log_tail()
            );
            thread::sleep(POLL);
        }
        Ok(())
    }

    fn log_tail(&self) -> String {
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n");
        format!("--- end of {} ---\n{tail}", self.log.display())
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // Both fail only when the process is already gone, which is the goal.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `N` distinct ports, each free on every address in `ips`.
pub(crate) fn free_ports<const N: usize>(ips: &[Ipv4Addr]) -> Result<[u16; N]> {
    // Holding every probe until all ports are picked keeps the picks distinct.
    let mut held = Vec::new();
    let mut ports = [0; N];
    for port in &mut ports {
        *port = (0..100)
            .find_map(|_| probe(ips, &mut held))
            .context("no port is free on all of the lab's loopback addresses")?;
    }
    Ok(ports)
}

fn probe(ips: &[Ipv4Addr], held: &mut Vec<TcpListener>) -> Option<u16> {
    let port = rand::random_range(PORTS);
    let listeners = ips
        .iter()
        .map(|ip| TcpListener::bind((*ip, port)))
        .collect::<io::Result<Vec<_>>>()
        .ok()?;
    held.extend(listeners);
    Some(port)
}

/// The path of a workspace binary, as built by `cargo build --bins`.
pub(crate) fn binary(name: &str) -> Result<PathBuf> {
    let target = env::var_os("CARGO_TARGET_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"),
        PathBuf::from,
    );
    let path = target.join("debug").join(name);
    ensure!(
        path.is_file(),
        "{} does not exist; run `cargo build --bins` first",
        path.display()
    );
    Ok(path)
}

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
pub(crate) const POLL: Duration = Duration::from_millis(50);
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

/// Runs a tool to completion and returns its standard output; fails with its standard error if
/// it fails.
pub fn run(command: &mut Command) -> Result<String> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("starting {:?}", command.get_program()))?;
    ensure!(
        output.status.success(),
        "{command:?}: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

/// The command line and environment visible for process `pid`.
pub fn process_snapshot(pid: u32) -> Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        let mut bytes = fs::read(format!("/proc/{pid}/cmdline"))?;
        bytes.extend(fs::read(format!("/proc/{pid}/environ"))?);
        Ok(bytes)
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Command::new("ps")
            .args(["eww", "-p", &pid.to_string(), "-o", "command="])
            .output()?
            .stdout)
    }
}

/// The pid of the process whose arguments contain `argument`.
pub fn process_with_arg(argument: &Path) -> Result<i32> {
    #[cfg(target_os = "linux")]
    {
        let wanted = argument.as_os_str().as_encoded_bytes();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry.file_name().to_str().and_then(|pid| pid.parse().ok()) else {
                continue;
            };
            let Ok(command) = fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            if command.split(|&byte| byte == 0).any(|arg| arg == wanted) {
                return Ok(pid);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let wanted = argument.to_string_lossy();
        let processes = run(Command::new("ps").args(["-axo", "pid=,command="]))?;
        for line in processes.lines() {
            let Some((pid, command)) = line.trim().split_once(char::is_whitespace) else {
                continue;
            };
            if command.split_whitespace().any(|arg| arg == wanted) {
                return Ok(pid.parse()?);
            }
        }
    }
    bail!("no process has argument {}", argument.display())
}

/// A process stopped with SIGSTOP until this guard is dropped.
pub struct StoppedProcess(i32);

impl StoppedProcess {
    pub fn new(pid: i32) -> Result<Self> {
        signal(pid, "-STOP")?;
        Ok(Self(pid))
    }
}

impl Drop for StoppedProcess {
    fn drop(&mut self) {
        let _ = signal(self.0, "-CONT");
    }
}

fn signal(pid: i32, signal: &str) -> Result<()> {
    let status = Command::new("kill").arg(signal).arg(pid.to_string()).status()?;
    ensure!(status.success(), "kill {signal} {pid}");
    Ok(())
}

/// `pid (command)` for each direct child of `parent`.
pub fn child_processes(parent: u32) -> Result<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        let parent = parent.to_string();
        let mut children = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let Ok(stat) = fs::read_to_string(entry?.path().join("stat")) else {
                continue;
            };
            let Some((process, rest)) = stat.rsplit_once(") ") else {
                continue;
            };
            if rest.split(' ').nth(1) == Some(parent.as_str()) {
                children.push(format!("{process})"));
            }
        }
        Ok(children)
    }
    #[cfg(target_os = "macos")]
    {
        let table = run(Command::new("ps").args(["-axo", "pid=,ppid=,comm="]))?;
        Ok(table
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?;
                let child = fields.next()?.parse::<u32>().ok()? == parent;
                let command = fields.collect::<Vec<_>>().join(" ");
                (child && command != "ps").then(|| format!("{pid} ({command})"))
            })
            .collect())
    }
}

/// User and system CPU time consumed by process `pid`.
pub fn process_cpu_time(pid: u32) -> Result<Duration> {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let (_, fields) = stat.rsplit_once(") ").context("malformed /proc stat")?;
        let fields: Vec<&str> = fields.split(' ').collect();
        let ticks: u64 = fields[11].parse::<u64>()? + fields[12].parse::<u64>()?;
        Ok(Duration::from_millis(ticks * 10))
    }
    #[cfg(target_os = "macos")]
    {
        let elapsed = run(Command::new("ps").args(["-p", &pid.to_string(), "-o", "time="]))?;
        let parts = elapsed.trim().split(':').collect::<Vec<_>>();
        let seconds = match parts.as_slice() {
            [minutes, seconds] => minutes.parse::<f64>()? * 60.0 + seconds.parse::<f64>()?,
            [hours, minutes, seconds] => {
                hours.parse::<f64>()? * 3600.0 + minutes.parse::<f64>()? * 60.0 + seconds.parse::<f64>()?
            }
            _ => bail!("unexpected ps CPU time {elapsed:?}"),
        };
        Ok(Duration::from_secs_f64(seconds))
    }
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

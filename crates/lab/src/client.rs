//! A FUSE client: its mount process, its directories and its stats.

use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use crate::issuer::Issuer;
use crate::jev::FakeJev;
use crate::process::{self, Proc};
use crate::store::S3_HOST;
use crate::{Profile, SPACE};

/// The cache limit of a client unless [`crate::LabConfig::cache_bytes`] sets another.
pub(crate) const CACHE_BYTES: u64 = 1 << 30;
const STATS_XATTR: &str = "user.herdfs.stats";
/// Long enough for a few commit retries, which back off from 1 s.
/// A 1 GiB file takes about a minute to commit on a loaded box.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(300);

/// The access strategy a client's mount uses (`--strategy`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    Stream,
    Fetch,
    Pin,
    Rule,
    /// Seeded random choices.
    Chaos(u64),
    Jev,
}

impl Strategy {
    fn arg(self) -> String {
        match self {
            Self::Stream => "stream".into(),
            Self::Fetch => "fetch".into(),
            Self::Pin => "pin".into(),
            Self::Rule => "rule".into(),
            Self::Chaos(seed) => format!("chaos:{seed}"),
            Self::Jev => "jev".into(),
        }
    }
}

pub(crate) struct ClientSpec {
    pub(crate) name: String,
    pub(crate) user: String,
    pub(crate) groups: Vec<String>,
    pub(crate) profile: Profile,
    pub(crate) strategy: Strategy,
    pub(crate) cache_bytes: u64,
}

pub(crate) struct Client {
    pub(crate) spec: ClientSpec,
    /// The space it mounts: [`SPACE`] unless a test chose another.
    pub(crate) space: String,
    /// Where this client's proxies listen.
    pub(crate) ip: Ipv4Addr,
    /// Cache, journal, token file and log.
    pub(crate) dir: PathBuf,
    pub(crate) mount: PathBuf,
    /// The fake Jev that a client deciding with Jev asks.
    pub(crate) jev: Option<FakeJev>,
    process: Option<Proc>,
}

impl Client {
    pub(crate) fn new(spec: ClientSpec, ip: Ipv4Addr, root: &Path) -> Result<Self> {
        let dir = root.join("clients").join(&spec.name);
        let mount = root.join("mnt").join(&spec.name);
        for path in [dir.join("cache"), dir.join("journal"), mount.clone()] {
            fs::create_dir_all(path)?;
        }
        let jev = (spec.strategy == Strategy::Jev)
            .then(FakeJev::start)
            .transpose()?;
        Ok(Self {
            jev,
            spec,
            space: SPACE.to_owned(),
            ip,
            dir,
            mount,
            process: None,
        })
    }

    /// The authority's base URL through this client's link.
    pub(crate) fn authority_url(&self, authority_port: u16) -> String {
        format!("http://{}:{authority_port}", self.ip)
    }

    /// Mounts its space with a fresh token, after killing a running mount.
    pub(crate) fn start(&mut self, issuer: &Issuer, authority_port: u16) -> Result<()> {
        self.kill()?;
        let token = self.dir.join("token");
        fs::write(&token, issuer.token(&self.spec.user, &self.spec.groups))?;
        let mut command = Command::new(process::binary(process::CLIENT)?);
        command
            .args(["mount", "--authority", &self.authority_url(authority_port)])
            .args(["--space", &self.space, "--token-file"])
            .arg(token)
            .arg("--cache")
            .arg(self.dir.join("cache"))
            .args(["--cache-bytes", &self.spec.cache_bytes.to_string(), "--journal"])
            .arg(self.dir.join("journal"))
            .args(["--strategy", &self.spec.strategy.arg()])
            .args(["--resolve", &format!("{S3_HOST}={}", self.ip)])
            .arg(&self.mount);
        if let Some(jev) = &self.jev {
            let key = self.dir.join("jev-key");
            fs::write(&key, "lab")?;
            command
                .arg("--jev-key-file")
                .arg(key)
                .args(["--jev-url", jev.url()]);
        }
        let mut mount = Proc::spawn(&self.spec.name, command, &self.dir.join("client.log"))?;
        mount.wait_until("mounted", || is_mounted(&self.mount))?;
        self.process = Some(mount);
        Ok(())
    }

    /// SIGKILLs the mount process, then detaches the mount it leaves behind.
    pub(crate) fn kill(&mut self) -> Result<()> {
        self.process = None;
        if !is_mounted(&self.mount) {
            return Ok(());
        }
        let status = Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(&self.mount)
            .status()?;
        ensure!(
            status.success(),
            "fusermount3 -u -z {}: {status}",
            self.mount.display()
        );
        Ok(())
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(Proc::id)
    }

    pub(crate) fn stats(&self) -> Result<Value> {
        let json = xattr::get(&self.mount, STATS_XATTR)?
            .with_context(|| format!("{} has no {STATS_XATTR}", self.mount.display()))?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// Waits until every write closed so far is committed or set aside.
    pub(crate) fn wait_committed(&self) -> Result<()> {
        let deadline = Instant::now() + COMMIT_TIMEOUT;
        while self.stats()?["pending_commits"] != 0 {
            ensure!(
                Instant::now() < deadline,
                "{} has pending commits after {COMMIT_TIMEOUT:?}",
                self.spec.name
            );
            thread::sleep(process::POLL);
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Err(error) = self.kill() {
            eprintln!("lab: {error:#}");
        }
    }
}

fn is_mounted(path: &Path) -> bool {
    let Ok(table) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    // The fifth field of each line is the mount point.
    table.lines().any(|line| line.split(' ').nth(4) == path.to_str())
}

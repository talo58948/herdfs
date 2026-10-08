//! A FUSE client: its mount process, its directories and its stats.

use std::fs;
use std::net::{Ipv4Addr, TcpStream};
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

/// Filesystem adapter a lab client mounts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mount {
    #[default]
    Fuse,
    Nfs,
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

#[derive(Clone)]
pub(crate) struct ClientSpec {
    pub(crate) name: String,
    pub(crate) user: String,
    pub(crate) groups: Vec<String>,
    pub(crate) profile: Profile,
    pub(crate) strategy: Strategy,
    pub(crate) cache_bytes: u64,
    pub(crate) mount: Mount,
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
        if self.spec.mount == Mount::Nfs {
            command.arg("--nfs");
        }
        if let Some(jev) = &self.jev {
            let key = self.dir.join("jev-key");
            fs::write(&key, "lab")?;
            command
                .arg("--jev-key-file")
                .arg(key)
                .args(["--jev-url", jev.url()]);
        }
        let mut mount = Proc::spawn(&self.spec.name, command, &self.dir.join("client.log"))?;
        if self.spec.mount == Mount::Nfs {
            mount.wait_until("serving NFS", || {
                nfs_port(&self.dir)
                    .is_some_and(|port| TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok())
            })?;
            mount_nfs(
                &self.mount,
                nfs_port(&self.dir).context("NFS server did not publish its port")?,
            )?;
        }
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
        let status = match self.spec.mount {
            Mount::Fuse => Command::new("fusermount3")
                .args(["-u", "-z"])
                .arg(&self.mount)
                .status()?,
            Mount::Nfs => Command::new("sudo")
                .args(["umount", "-f"])
                .arg(&self.mount)
                .status()?,
        };
        ensure!(status.success(), "unmounting {}: {status}", self.mount.display());
        Ok(())
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(Proc::id)
    }

    pub(crate) fn stats(&self) -> Result<Value> {
        ensure!(self.spec.mount == Mount::Fuse, "stats xattr is FUSE-only");
        let json = xattr::get(&self.mount, STATS_XATTR)?
            .with_context(|| format!("{} has no {STATS_XATTR}", self.mount.display()))?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// Waits until every write closed so far is committed or set aside.
    pub(crate) fn wait_committed(&self) -> Result<()> {
        let deadline = Instant::now() + COMMIT_TIMEOUT;
        while self.pending()? {
            ensure!(
                Instant::now() < deadline,
                "{} has pending commits after {COMMIT_TIMEOUT:?}",
                self.spec.name
            );
            thread::sleep(process::POLL);
        }
        Ok(())
    }

    fn pending(&self) -> Result<bool> {
        if self.spec.mount == Mount::Fuse {
            return Ok(self.stats()?["pending_commits"] != 0);
        }
        // Journal records are JSON files in this directory; NFS identity is journal.nfs.json
        // beside it. This is deliberately the lab's on-disk format probe for mounts without xattrs.
        let records = fs::read_dir(self.dir.join("journal"))?;
        Ok(records.filter_map(|record| record.ok()).any(|record| {
            record
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        }))
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

fn nfs_port(dir: &Path) -> Option<u16> {
    let identity = fs::read(dir.join("journal.nfs.json")).ok()?;
    let identity: Value = serde_json::from_slice(&identity).ok()?;
    u16::try_from(identity["port"].as_u64()?).ok()
}

fn mount_nfs(mountpoint: &Path, port: u16) -> Result<()> {
    let options = format!(
        "vers=3,tcp,nolock,noacl,actimeo=1,lookupcache=none,rsize=131072,wsize=131072,port={port},mountport={port}"
    );
    let status = Command::new("sudo")
        .arg("mount.nfs")
        .args(["-o", &options, "127.0.0.1:/"])
        .arg(mountpoint)
        .status()?;
    ensure!(status.success(), "mounting {}: {status}", mountpoint.display());
    Ok(())
}

//! A test lab for herdfs on one machine, without root: MinIO, toxiproxy, a test OIDC
//! issuer, the authority, and FUSE clients. Client N reaches the authority and the store through
//! its own proxies on 127.0.0.N, shaped by its link profile.
//!
//! The lab is synchronous; use it from plain `#[test]` functions. Dropping it kills every process
//! and detaches every mount. `LAB_KEEP=1` keeps its directory, with all logs, for debugging.

mod authority;
mod chunks;
mod client;
mod direct;
mod issuer;
mod jev;
mod lifecycle;
mod mount;
mod process;
mod store;
mod sweep;
mod toxiproxy;

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use serde_json::Value;
use tempfile::TempDir;

use crate::authority::Authority;
use crate::client::Client;
use crate::process::Proc;
use crate::store::Store;
use crate::toxiproxy::Toxiproxy;

pub use crate::authority::Collection;
pub use crate::chunks::overlapping_chunk_bytes;
pub use crate::client::Strategy;
pub use crate::issuer::{AUDIENCE, BadToken, Issuer};
pub use crate::jev::{FakeJev, JevAnswers, JevMode};
pub use crate::lifecycle::LabConfig;
pub use crate::mount::{abort_mount, mounted};
pub use crate::process::{
    StoppedProcess, child_processes, process_cpu_time, process_snapshot, process_with_arg, run,
};
pub use crate::store::STORE_CREDENTIALS;
pub use crate::sweep::{Hit, canary, sweep};
pub use crate::toxiproxy::{Links, Profile};

/// The space every client mounts.
pub const SPACE: &str = "lab";
/// The user who creates [`SPACE`] and grants each client's user `write` on its root.
pub const ADMIN: &str = "admin";

/// A running lab. Dropping it tears everything down.
pub struct Lab {
    // Fields drop in order: plugins, then mounts, then the services they use, the directory last.
    agents: Vec<Proc>,
    clients: Vec<Client>,
    authority: Authority,
    toxiproxy: Toxiproxy,
    store: Store,
    issuer: Issuer,
    store_port: u16,
    dir: TempDir,
}

impl Lab {
    pub fn mount_path(&self, name: &str) -> &Path {
        &self.client(name).mount
    }

    /// Client `name`'s cache, journal, token file and log: what an attacker on its host holds.
    pub fn client_dir(&self, name: &str) -> &Path {
        &self.client(name).dir
    }

    /// The pid of client `name`'s mount process, while it runs.
    pub fn client_pid(&self, name: &str) -> Option<u32> {
        self.client(name).pid()
    }

    /// The pid of the authority process, while it runs.
    pub fn authority_pid(&self) -> Option<u32> {
        self.authority.pid()
    }

    /// The authority's base URL as client `name` reaches it, through its link.
    pub fn authority_url(&self, name: &str) -> String {
        self.client(name).authority_url(self.authority.port)
    }

    /// How many refresh-token grants the test issuer has served.
    pub fn issuer_refresh_count(&self) -> u64 {
        self.issuer.refresh_count()
    }

    /// Client `name`'s counters, from the `user.herdfs.stats` xattr on its mount root.
    pub fn stats(&self, name: &str) -> Result<Value> {
        self.client(name).stats()
    }

    /// The latest access plan for `path`, from its `user.herdfs.plan` xattr.
    pub fn plan(&self, name: &str, path: &Path) -> Result<Value> {
        self.client(name).plan(path)
    }

    /// Waits until client `name` has committed, or set aside, every write closed so far.
    pub fn wait_committed(&self, name: &str) -> Result<()> {
        self.client(name).wait_committed()
    }

    /// Takes client `name`'s links down; connects to them are refused.
    pub fn cut(&self, name: &str, links: Links) -> Result<()> {
        self.toxiproxy.set_links(name, links, false)
    }

    /// Makes client `name`'s links drop everything silently, as a real outage does.
    pub fn silence(&self, name: &str, links: Links) -> Result<()> {
        self.toxiproxy.silence(name, links, true)
    }

    /// Brings back links that were cut or silenced.
    pub fn restore(&self, name: &str, links: Links) -> Result<()> {
        self.toxiproxy.set_links(name, links, true)?;
        self.toxiproxy.silence(name, links, false)
    }

    fn client(&self, name: &str) -> &Client {
        let client = self.clients.iter().find(|client| client.spec.name == name);
        client.unwrap_or_else(|| no_client(name))
    }

    fn client_mut(&mut self, name: &str) -> &mut Client {
        let client = self.clients.iter_mut().find(|client| client.spec.name == name);
        client.unwrap_or_else(|| no_client(name))
    }
}

/// Why the lab cannot run yet, if a workspace binary it runs is not built.
pub fn missing_binaries() -> Option<String> {
    [process::AUTHORITY, process::CLIENT, process::AGENTS]
        .into_iter()
        .find_map(|name| process::binary(name).err())
        .map(|error| error.to_string())
}

/// A command for the workspace's `herdfs` binary.
pub fn herdfs_command() -> Result<Command> {
    Ok(Command::new(process::binary(process::CLIENT)?))
}

/// A client name the lab does not know is a bug in the test.
fn no_client(name: &str) -> ! {
    panic!("the lab has no client named {name}")
}

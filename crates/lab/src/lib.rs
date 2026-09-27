//! A test lab for herdfs on one Linux machine, without root: MinIO, toxiproxy, a test OIDC
//! issuer, the authority, and FUSE clients. Client N reaches the authority and the store through
//! its own proxies on 127.0.0.N, shaped by its link profile.
//!
//! The lab is synchronous; use it from plain `#[test]` functions. Dropping it kills every process
//! and detaches every mount. `LAB_KEEP=1` keeps its directory, with all logs, for debugging.

mod authority;
mod chunks;
mod client;
mod issuer;
mod process;
mod store;
mod sweep;
mod toxiproxy;

use std::env;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use herdfs_core::SpacePath;
use herdfs_core::proto::{self, Perm, Request, Response};
use reqwest::Url;
use reqwest::blocking::Client as Http;
use serde_json::Value;
use tempfile::TempDir;

use crate::authority::Authority;
use crate::client::{CACHE_BYTES, Client, ClientSpec};
use crate::issuer::Issuer;
use crate::process::Proc;
use crate::store::{S3_HOST, Store};
use crate::toxiproxy::Toxiproxy;

pub use crate::chunks::overlapping_chunk_bytes;
pub use crate::client::Strategy;
pub use crate::issuer::BadToken;
pub use crate::process::run;
pub use crate::store::STORE_CREDENTIALS;
pub use crate::sweep::{Hit, canary, sweep};
pub use crate::toxiproxy::{Links, Profile};

/// The space every client mounts.
pub const SPACE: &str = "lab";
/// The user who creates [`SPACE`] and grants each client's user `write` on its root.
pub const ADMIN: &str = "admin";

/// The clients of a lab.
#[derive(Default)]
pub struct LabConfig {
    clients: Vec<ClientSpec>,
    on_tmpfs: bool,
}

impl LabConfig {
    /// Adds client `name`, which mounts as `user` in `groups`.
    pub fn client(
        mut self,
        name: &str,
        user: &str,
        groups: &[&str],
        profile: Profile,
        strategy: Strategy,
    ) -> Self {
        assert!(
            self.clients.iter().all(|client| client.name != name),
            "two clients named {name}"
        );
        let groups = groups.iter().map(|&group| group.to_owned()).collect();
        self.clients.push(ClientSpec {
            name: name.to_owned(),
            user: user.to_owned(),
            groups,
            profile,
            strategy,
            cache_bytes: CACHE_BYTES,
        });
        self
    }

    /// Keeps the lab's directory in `/dev/shm`, so that many changes do not each wait on fsync.
    pub fn on_tmpfs(mut self) -> Self {
        self.on_tmpfs = true;
        self
    }

    /// Limits client `name`'s chunk cache to `bytes` instead of 1 GiB.
    pub fn cache_bytes(mut self, name: &str, bytes: u64) -> Self {
        let client = self.clients.iter_mut().find(|client| client.name == name);
        client.unwrap_or_else(|| no_client(name)).cache_bytes = bytes;
        self
    }
}

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
    /// Starts the whole lab: [`Lab::start_infra`], then the authority, then [`SPACE`] with each
    /// client's user granted `write` on its root, then every mount.
    pub fn start(config: LabConfig) -> Result<Self> {
        process::binary(process::AUTHORITY)?;
        process::binary(process::CLIENT)?;
        let mut lab = Self::start_infra(config)?;
        lab.restart_authority()?;
        lab.rpc(ADMIN, &Request::CreateSpace { space: SPACE.into() })?;
        for client in &lab.clients {
            let principal = format!("user:{}", client.spec.user);
            let grant = Request::SetAcl {
                space: SPACE.into(),
                path: SpacePath::root(),
                principal,
                perm: Some(Perm::Write),
            };
            lab.rpc(ADMIN, &grant)?;
        }
        for client in &mut lab.clients {
            client.start(&lab.issuer, lab.authority.port)?;
        }
        Ok(lab)
    }

    /// Starts MinIO, toxiproxy, the issuer and every client's proxies, but neither the
    /// authority nor any mount.
    pub fn start_infra(config: LabConfig) -> Result<Self> {
        let parent = config.on_tmpfs.then(|| "/dev/shm".into());
        let parent = parent.unwrap_or_else(env::temp_dir);
        let mut dir = tempfile::Builder::new()
            .prefix("herdfs-lab-")
            .tempdir_in(parent)?;
        if env::var("LAB_KEEP").is_ok_and(|keep| keep == "1") {
            dir.disable_cleanup(true);
            eprintln!("lab: keeping {}", dir.path().display());
        }
        // The services listen on 127.0.0.1; client N, counting from 2, has its proxies on 127.0.0.N.
        let loopback: Vec<Ipv4Addr> = (1..=u8::MAX)
            .map(|n| Ipv4Addr::new(127, 0, 0, n))
            .take(config.clients.len() + 1)
            .collect();
        let [store_port, authority_port, toxiproxy_port] = process::free_ports(&loopback)?;
        let http = Http::builder()
            .resolve(S3_HOST, (Ipv4Addr::LOCALHOST, store_port).into())
            .build()?;
        let store = Store::start(dir.path(), store_port, &http)?;
        let toxiproxy = Toxiproxy::start(dir.path(), toxiproxy_port, http.clone())?;
        let issuer = Issuer::start()?;
        let clients = (config.clients.into_iter().zip(&loopback[1..]))
            .map(|(spec, &ip)| Client::new(spec, ip, dir.path()))
            .collect::<Result<Vec<_>>>()?;
        for client in &clients {
            let spec = &client.spec;
            toxiproxy.add_links(&spec.name, client.ip, authority_port, store_port, spec.profile)?;
        }
        Ok(Self {
            agents: Vec::new(),
            clients,
            authority: Authority::new(authority_port, http),
            toxiproxy,
            store,
            issuer,
            store_port,
            dir,
        })
    }

    /// SIGKILLs the authority.
    pub fn kill_authority(&mut self) {
        self.authority.kill();
    }

    /// Starts the authority on its database, after killing a running one, and waits until
    /// `/v1/health` answers.
    pub fn restart_authority(&mut self) -> Result<()> {
        self.authority
            .restart(self.dir.path(), self.store_port, &self.issuer)
    }

    /// SIGKILLs client `name`'s mount process and detaches its mount.
    pub fn kill_client(&mut self, name: &str) -> Result<()> {
        self.client_mut(name).kill()
    }

    /// Mounts client `name` again, after killing it if it runs. Its cache and journal persist.
    pub fn restart_client(&mut self, name: &str) -> Result<()> {
        let client = self.clients.iter_mut().find(|client| client.spec.name == name);
        let client = client.unwrap_or_else(|| no_client(name));
        client.start(&self.issuer, self.authority.port)
    }

    /// Mounts client `name` on `space` from now on, such as a fork, after killing it if it runs.
    pub fn mount_space(&mut self, name: &str, space: &str) -> Result<()> {
        space.clone_into(&mut self.client_mut(name).space);
        self.restart_client(name)
    }

    /// Runs `herdfs-agents` for host `name` on client `name`'s mount until the lab is dropped.
    pub fn start_agents(&mut self, name: &str, command: &str) -> Result<()> {
        let client = self.client(name);
        let mut runner = Command::new(process::binary(process::AGENTS)?);
        runner
            .arg("--mount")
            .arg(&client.mount)
            .args(["--host", name, "--command", command]);
        let runner = Proc::spawn("agents", runner, &client.dir.join("agents.log"))?;
        self.agents.push(runner);
        Ok(())
    }

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

    /// The authority's base URL as client `name` reaches it, through its link.
    pub fn authority_url(&self, name: &str) -> String {
        self.client(name).authority_url(self.authority.port)
    }

    /// Client `name`'s counters, from the `user.herdfs.stats` xattr on its mount root.
    pub fn stats(&self, name: &str) -> Result<Value> {
        self.client(name).stats()
    }

    /// Waits until client `name` has committed, or set aside, every write closed so far.
    pub fn wait_committed(&self, name: &str) -> Result<()> {
        self.client(name).wait_committed()
    }

    /// Takes client `name`'s links down; connects to them are refused.
    pub fn cut(&self, name: &str, links: Links) -> Result<()> {
        self.toxiproxy.set_links(name, links, false)
    }

    pub fn restore(&self, name: &str, links: Links) -> Result<()> {
        self.toxiproxy.set_links(name, links, true)
    }

    /// Calls the authority directly as `user`, with the groups of the first client that mounts
    /// as `user`, if any.
    pub fn rpc(&self, user: &str, request: &Request) -> Result<Response, proto::Error> {
        let client = self.clients.iter().find(|client| client.spec.user == user);
        let groups = client
            .map(|client| client.spec.groups.as_slice())
            .unwrap_or_default();
        self.rpc_with(&self.issuer.token(user, groups), request)
    }

    /// Calls the authority directly with any token, such as a [`BadToken`].
    pub fn rpc_with(&self, token: &str, request: &Request) -> Result<Response, proto::Error> {
        self.authority.rpc(token, request)
    }

    /// A token from the lab's issuer for `user` in `groups`.
    pub fn token(&self, user: &str, groups: &[&str]) -> String {
        self.issuer.token(user, groups)
    }

    pub fn bad_token(&self, user: &str, kind: BadToken) -> String {
        self.issuer.bad_token(user, kind)
    }

    pub fn issuer_url(&self) -> &str {
        self.issuer.url()
    }

    /// An HTTP client as on client `name`'s host: `s3.lab` resolves to its store link.
    pub fn http(&self, name: &str) -> Result<Http> {
        let link = SocketAddr::from((self.client(name).ip, self.store_port));
        Ok(Http::builder().resolve(S3_HOST, link).build()?)
    }

    /// A presigned GET for `key` in the bucket, naming host `s3.lab` as the authority's grants do.
    pub fn presign_get(&self, key: &str) -> Url {
        self.store.presign_get(key)
    }

    /// A presigned PUT for `key` in the bucket, naming host `s3.lab`.
    pub fn presign_put(&self, key: &str) -> Url {
        self.store.presign_put(key)
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

/// A client name the lab does not know is a bug in the test.
fn no_client(name: &str) -> ! {
    panic!("the lab has no client named {name}")
}

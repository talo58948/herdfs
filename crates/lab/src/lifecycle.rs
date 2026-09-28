//! Starting a lab and killing or restarting its processes.

use std::env;
use std::net::Ipv4Addr;
use std::process::Command;

use anyhow::Result;
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request};
use reqwest::blocking::Client as Http;

use crate::authority::Authority;
use crate::client::{CACHE_BYTES, Client, ClientSpec};
use crate::issuer::Issuer;
use crate::process::{self, Proc};
use crate::store::{S3_HOST, Store};
use crate::toxiproxy::Toxiproxy;
use crate::{ADMIN, Lab, Profile, SPACE, Strategy, no_client};

/// Tries per lab start: a port picked free is taken only when its service binds it, and a
/// parallel lab may take it first.
const PORT_TRIES: usize = 3;

/// The clients of a lab.
#[derive(Clone, Default)]
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

impl Lab {
    /// Starts the whole lab: [`Lab::start_infra`], then the authority, then [`SPACE`] with each
    /// client's user granted `write` on its root, then every mount.
    pub fn start(config: LabConfig) -> Result<Self> {
        process::binary(process::AUTHORITY)?;
        process::binary(process::CLIENT)?;
        let mut lab = with_fresh_ports(config, |config| {
            let mut lab = Self::start_services(config)?;
            lab.restart_authority()?;
            Ok(lab)
        })?;
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
        with_fresh_ports(config, Self::start_services)
    }

    fn start_services(config: LabConfig) -> Result<Self> {
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
}

/// Runs `start` again, on fresh ports, when a service could not bind the port picked for it.
fn with_fresh_ports(config: LabConfig, start: impl Fn(LabConfig) -> Result<Lab>) -> Result<Lab> {
    for _ in 1..PORT_TRIES {
        match start(config.clone()) {
            Err(error) if format!("{error:#}").contains("already in use") => {
                eprintln!("lab: a port was taken before its service bound it; starting again\n{error:#}");
            }
            started => return started,
        }
    }
    start(config)
}

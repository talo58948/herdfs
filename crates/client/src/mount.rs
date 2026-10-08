//! `herdfs mount`: connects to the authority, resumes pending commits and serves a filesystem.

use std::{net::IpAddr, path::PathBuf, sync::Arc};

use crate::{
    cache::Cache,
    fs::{Inodes, Kernel, Space},
    journal::{Committer, Journal},
    remote::Remote,
    stats::Stats,
    strategy::{Decider, Jev, StrategyArg},
};
use anyhow::Context;
use herdfs_core::SpacePath;
#[cfg(target_os = "linux")]
use {
    crate::fs::Fuse,
    fuser::{Config, MountOption, Session},
};
#[cfg(unix)]
use {
    crate::fs::{Identity, Nfs},
    nfsserve::tcp::{NFSTcp, NFSTcpListener},
    std::{fs, os::unix::fs::PermissionsExt, process::Command},
    tokio::signal::unix::{SignalKind, signal},
};

/// Flags of `herdfs mount`.
#[derive(clap::Args)]
pub struct MountArgs {
    /// Authority base URL, e.g. http://127.0.0.1:7000.
    #[arg(long)]
    authority: String,
    /// The space to mount.
    #[arg(long)]
    space: String,
    /// File holding the bearer token, re-read when the authority answers 401.
    #[arg(long)]
    token_file: PathBuf,
    /// Chunk cache directory.
    #[arg(long)]
    cache: PathBuf,
    /// Cache size limit in bytes. Pinned chunks may exceed it.
    #[arg(long)]
    cache_bytes: u64,
    /// Directory for writes not yet committed.
    #[arg(long)]
    journal: PathBuf,
    /// stream, fetch, pin, rule (fetch below 64 MiB, else stream), chaos:SEED or jev.
    #[arg(long, default_value = "rule")]
    strategy: StrategyArg,
    /// File holding the Vercel AI Gateway key, for `--strategy jev`.
    #[arg(long, required_if_eq("strategy", "jev"))]
    jev_key_file: Option<PathBuf>,
    /// Jev's evaluation endpoint.
    #[arg(long, default_value = "https://ai-gateway.vercel.sh/v1/evaluate")]
    jev_url: String,
    /// HOST=IP: connect to IP for HOST, keeping the URL's port and host, like curl's --resolve.
    #[arg(long, value_parser = parse_resolve)]
    resolve: Vec<(String, IpAddr)>,
    /// Serve loopback NFSv3 instead of FUSE (the default on macOS).
    #[arg(long, default_value_t = cfg!(target_os = "macos"))]
    nfs: bool,
    mountpoint: PathBuf,
}

/// Mounts the space and serves it until it is unmounted or the process gets SIGINT or SIGTERM.
pub fn mount(args: &MountArgs) -> anyhow::Result<()> {
    if args.nfs {
        return mount_nfs(args);
    }
    mount_fuse(args)
}

#[cfg(target_os = "linux")]
fn mount_fuse(args: &MountArgs) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let space = runtime.block_on(connect(args))?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("herdfs".into())];
    let fuse = Fuse::new(space.clone(), runtime.handle().clone());
    let mut session = Session::new(fuse, &args.mountpoint, &config)
        .with_context(|| format!("mounting at {}", args.mountpoint.display()))?;
    Fuse::mounted(&space, session.notifier());
    let mut unmounter = session.unmount_callable();
    runtime.spawn(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        if let Err(err) = unmounter.unmount() {
            tracing::warn!(%err, "unmount failed");
        }
    });
    tracing::info!(mountpoint = %args.mountpoint.display(), space = args.space, "mounted");
    session.run()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn mount_fuse(_args: &MountArgs) -> anyhow::Result<()> {
    anyhow::bail!("FUSE mounting is supported only on Linux; use --nfs")
}

async fn connect(args: &MountArgs) -> anyhow::Result<Arc<Space>> {
    let (authority, space, token_file) = (&args.authority, &args.space, &args.token_file);
    let remote = Remote::new(authority, space, token_file.clone(), &args.resolve)
        .with_context(|| format!("reading token file {}", token_file.display()))?;
    let root = remote.stat(SpacePath::root()).await;
    let root = root.with_context(|| format!("cannot reach space {space:?} at the authority {authority}"))?;
    let cache = Cache::open(args.cache.clone(), args.cache_bytes)
        .with_context(|| format!("opening cache {}", args.cache.display()))?;
    let inodes = Arc::new(Inodes::default());
    let (journal, woken) = Journal::open(args.journal.clone(), &inodes)
        .with_context(|| format!("opening journal {}", args.journal.display()))?;
    let (remote, cache, journal) = (Arc::new(remote), Arc::new(cache), Arc::new(journal));
    let stats = Arc::new(Stats::default());
    let decider = match &args.strategy {
        StrategyArg::Decider(decider) => decider.clone(),
        StrategyArg::Jev => {
            let key_file = args.jev_key_file.as_ref();
            let key_file = key_file.context("--strategy jev needs --jev-key-file")?;
            let jev = Jev::new(key_file, &args.jev_url);
            let jev = jev.with_context(|| format!("reading the Jev key file {}", key_file.display()))?;
            Decider::Jev(Arc::new(jev), stats.clone())
        }
    };
    let kernel = Arc::new(Kernel::new(root.id));
    let space = Space::new(remote.clone(), cache, journal, stats, decider, kernel, inodes);
    let space = Arc::new(if args.nfs {
        space.without_metadata_cache()
    } else {
        space
    });
    let committer = Committer { space: space.clone() };
    tokio::spawn(Arc::new(committer).run(woken));
    tokio::spawn(remote.probe());
    Ok(space)
}

#[cfg(unix)]
fn mount_nfs(args: &MountArgs) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let space = runtime.block_on(connect(args))?;
    let identity = Arc::new(Identity::open(&args.journal)?);
    let address = format!("127.0.0.1:{}", identity.port());
    let nfs = Nfs::new(space.clone(), space.kernel.root(), identity.clone());
    let listener = runtime
        .block_on(NFSTcpListener::bind(&address, nfs))
        .with_context(|| format!("binding the loopback NFS server at {address}"))?;
    let port = listener.get_listen_port();
    if identity.port() == 0 {
        identity.set_port(port)?;
    }
    fs::create_dir_all(&args.mountpoint)?;
    fs::set_permissions(&args.mountpoint, fs::Permissions::from_mode(0o700))?;
    let mut server = runtime.spawn(async move { listener.handle_forever().await });
    if let Err(error) = platform_mount(&args.mountpoint, port) {
        server.abort();
        return Err(error);
    }
    tracing::info!(mountpoint = %args.mountpoint.display(), space = args.space, port, "mounted");
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let stopped = runtime.block_on(async {
        tokio::select! {
            _ = interrupt.recv() => Ok(()),
            _ = terminate.recv() => Ok(()),
            result = &mut server => Err(anyhow::anyhow!("NFS server stopped: {result:?}")),
        }
    });
    let unmounted = platform_unmount(&args.mountpoint, false)
        .or_else(|error| platform_unmount(&args.mountpoint, true).context(error));
    server.abort();
    stopped?;
    unmounted
}

#[cfg(not(unix))]
fn mount_nfs(_args: &MountArgs) -> anyhow::Result<()> {
    anyhow::bail!("NFS mounting is supported only on Unix")
}

#[cfg(target_os = "macos")]
fn platform_mount(mountpoint: &std::path::Path, port: u16) -> anyhow::Result<()> {
    let options = format!(
        "vers=3,tcp,hard,intr,locallocks,noquota,rdirplus,actimeo=1,nonegnamecache,readahead=0,rsize=131072,wsize=131072,port={port},mountport={port}"
    );
    run_mount(
        Command::new("/sbin/mount_nfs")
            .args(["-o", &options, "127.0.0.1:/"])
            .arg(mountpoint),
    )
}

#[cfg(target_os = "linux")]
fn platform_mount(mountpoint: &std::path::Path, port: u16) -> anyhow::Result<()> {
    let options = format!(
        "vers=3,tcp,nolock,noacl,actimeo=1,lookupcache=none,rsize=131072,wsize=131072,port={port},mountport={port}"
    );
    run_mount(
        Command::new("sudo")
            .arg("mount.nfs")
            .args(["-o", &options, "127.0.0.1:/"])
            .arg(mountpoint),
    )
}

#[cfg(unix)]
fn platform_unmount(mountpoint: &std::path::Path, force: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("/sbin/umount");
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("sudo");
        command.arg("umount");
        command
    };
    if force {
        command.arg("-f");
    }
    run_mount(command.arg(mountpoint))
}

#[cfg(unix)]
fn run_mount(command: &mut Command) -> anyhow::Result<()> {
    let shown = format!("{command:?}");
    let status = command.status().with_context(|| format!("running {shown}"))?;
    anyhow::ensure!(status.success(), "{shown} failed with {status}");
    Ok(())
}

fn parse_resolve(value: &str) -> Result<(String, IpAddr), String> {
    let (host, ip) = value.split_once('=').ok_or("expected HOST=IP")?;
    let ip = ip.parse().map_err(|_| format!("invalid IP address {ip:?}"))?;
    Ok((host.to_owned(), ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_takes_host_equals_ip() {
        let parsed = parse_resolve("s3.lab=127.0.0.2").unwrap();
        assert_eq!(parsed, ("s3.lab".to_owned(), IpAddr::from([127, 0, 0, 2])));
        assert!(parse_resolve("s3.lab:9000:127.0.0.2").is_err());
    }
}

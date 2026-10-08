//! `herdfs mount`: connects to the authority, resumes pending commits and serves a filesystem.

use std::{net::IpAddr, path::PathBuf, sync::Arc};

use crate::{
    cache::Cache,
    fs::{Inodes, Kernel, Space},
    journal::{Committer, Journal},
    kept::Kept,
    remote::Remote,
    stats::Stats,
    strategy::{Decider, Jev, StrategyArg},
};
use anyhow::Context;
use herdfs_core::SpacePath;
use tokio::sync::watch;
#[cfg(target_os = "linux")]
use {
    crate::fs::Fuse,
    fuser::{Config, MountOption, Session},
    tokio::signal::unix::{SignalKind, signal},
};

#[cfg(unix)]
mod nfs;

/// Flags of `herdfs mount`.
#[derive(clap::Args)]
pub struct MountArgs {
    /// Authority base URL, e.g. http://127.0.0.1:7000.
    #[arg(long)]
    authority: String,
    /// The space to mount.
    #[arg(long)]
    space: String,
    /// Login token file, refreshed shortly before expiry.
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// Chunk cache directory.
    #[arg(long)]
    cache: PathBuf,
    /// Cache size limit in bytes. Pinned chunks may exceed it.
    #[arg(long)]
    cache_bytes: u64,
    /// Directory for writes not yet committed.
    #[arg(long)]
    journal: PathBuf,
    /// A named plan, explicit PREFETCH/READ_AHEAD/KEEP, rule, chaos:SEED or jev.
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

struct Connected {
    space: Arc<Space>,
    journal: PathBuf,
    state: PathBuf,
}

/// Mounts the space and serves it until it is unmounted or the process gets SIGINT or SIGTERM.
pub fn mount(args: &MountArgs) -> anyhow::Result<()> {
    if args.nfs {
        #[cfg(unix)]
        return nfs::mount(args);
        #[cfg(not(unix))]
        anyhow::bail!("NFS serving is supported only on Unix");
    }
    mount_fuse(args)
}

#[cfg(target_os = "linux")]
fn mount_fuse(args: &MountArgs) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let Connected { space, state, .. } = runtime.block_on(connect(args))?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("herdfs".into())];
    let fuse = Fuse::new(space.clone(), runtime.handle().clone());
    let _control = serve_control(args, space.clone(), &state)?;
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

fn serve_control(
    args: &MountArgs,
    space: Arc<Space>,
    state: &std::path::Path,
) -> anyhow::Result<crate::control::Guard> {
    crate::control::serve(space, &args.authority, &args.space, &args.mountpoint, state)
}

async fn connect(args: &MountArgs) -> anyhow::Result<Connected> {
    let (authority, space) = (&args.authority, &args.space);
    let token_file = args
        .token_file
        .clone()
        .map_or_else(crate::token::default_path, Ok)?;
    crate::token::read_access_token(&token_file)
        .with_context(|| format!("reading token file {}", token_file.display()))?;
    let remote = Remote::new(authority, space, token_file, &args.resolve)?;
    let root = remote.stat(SpacePath::root()).await;
    let root = root.with_context(|| format!("cannot reach space {space:?} at the authority {authority}"))?;
    let journal = std::path::absolute(&args.journal)?;
    let parent = journal
        .parent()
        .context("--journal must have a parent directory")?;
    let name = journal.file_name().context("--journal must name a directory")?;
    let mut state_name = name.to_os_string();
    state_name.push(".state");
    let state = parent.join(state_name);
    let pins = state.join("pins.json");
    let loaded = crate::kept::load(&pins).with_context(|| format!("reading pin state {}", pins.display()))?;
    let cache = Cache::open(args.cache.clone(), args.cache_bytes, &loaded.pinned)
        .with_context(|| format!("opening cache {}", args.cache.display()))?;
    let inodes = Arc::new(Inodes::default());
    let (opened_journal, woken) = Journal::open(journal.clone(), &inodes)
        .with_context(|| format!("opening journal {}", args.journal.display()))?;
    let (remote, cache, opened_journal) = (Arc::new(remote), Arc::new(cache), Arc::new(opened_journal));
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
    let kept = Arc::new(Kept::new(pins, loaded));
    let (commits, _) = watch::channel(false);
    let space = Space::new(
        remote.clone(),
        cache,
        opened_journal,
        commits,
        kept.clone(),
        stats,
        decider,
        kernel,
        inodes,
    );
    let space = Arc::new(if args.nfs {
        space.without_metadata_cache()
    } else {
        space
    });
    let committer = Committer { space: space.clone() };
    tokio::spawn(Arc::new(committer).run(woken));
    tokio::spawn(kept.run(space.clone()));
    tokio::spawn(remote.probe());
    Ok(Connected {
        space,
        journal,
        state,
    })
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

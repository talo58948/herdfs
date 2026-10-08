//! `herdfs mount`: connects to the authority, resumes pending commits and serves FUSE.

use std::{net::IpAddr, path::PathBuf};

use crate::strategy::StrategyArg;
#[cfg(target_os = "linux")]
use {
    crate::{
        cache::Cache,
        fs::{Fuse, Inodes, Kernel, MountState, Space},
        journal::CommitGate,
        journal::{Committer, Journal},
        kept::Kept,
        reader::Reader,
        remote::Remote,
        stats::Stats,
        strategy::{Decider, Jev},
    },
    anyhow::Context,
    fuser::{Config, MountOption, Session},
    herdfs_core::SpacePath,
    std::sync::Arc,
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
    mountpoint: PathBuf,
}

/// Mounts the space and serves it until it is unmounted or the process gets SIGINT or SIGTERM.
#[cfg(target_os = "linux")]
pub fn mount(args: &MountArgs) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let space = runtime.block_on(connect(args))?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("herdfs".into())];
    let fuse = Fuse::new(space.clone(), runtime.handle().clone());
    let journal = crate::control::absolute(&args.journal)?;
    let state = journal
        .parent()
        .context("--journal must have a parent directory")?;
    let _control = crate::control::serve(
        space.clone(),
        &args.authority,
        &args.space,
        &args.mountpoint,
        state,
    )?;
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
pub fn mount(_args: &MountArgs) -> anyhow::Result<()> {
    anyhow::bail!("mounting is not yet supported on this platform")
}

#[cfg(target_os = "linux")]
async fn connect(args: &MountArgs) -> anyhow::Result<Arc<Space>> {
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
    let journal = crate::control::absolute(&args.journal)?;
    let state = journal
        .parent()
        .context("--journal must have a parent directory")?;
    let pins = state.join("pins.json");
    let loaded = Kept::load(&pins).with_context(|| format!("reading pin state {}", pins.display()))?;
    let cache = Cache::open_pinned(args.cache.clone(), args.cache_bytes, &loaded.pinned)
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
    let kept = Arc::new(Kept::new(
        pins,
        loaded,
        remote.clone(),
        Arc::new(Reader::new(remote.clone(), cache.clone(), stats.clone())),
        cache.clone(),
        inodes.clone(),
        kernel.clone(),
    ));
    let commits = Arc::new(CommitGate::default());
    let space = Arc::new(Space::new(
        remote.clone(),
        cache,
        MountState {
            journal,
            commits,
            kept: Some(kept.clone()),
        },
        stats,
        decider,
        kernel,
        inodes,
    ));
    let committer = Committer { space: space.clone() };
    tokio::spawn(Arc::new(committer).run(woken));
    tokio::spawn(kept.run());
    tokio::spawn(remote.probe());
    Ok(space)
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

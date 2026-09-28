//! `herdfs mount`: connects to the authority, resumes pending commits and serves FUSE.

use std::{net::IpAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use fuser::{Config, MountOption, Session};
use herdfs_core::SpacePath;
use tokio::signal::unix::{SignalKind, signal};

use crate::fs::{Fuse, Space};
use crate::journal::{Committer, Journal};
use crate::strategy::{Decider, Jev, StrategyArg};
use crate::{cache::Cache, remote::Remote, stats::Stats};

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
    mountpoint: PathBuf,
}

/// Mounts the space and serves it until it is unmounted or the process gets SIGINT or SIGTERM.
pub fn mount(args: &MountArgs) -> anyhow::Result<()> {
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
    space.set_notifier(session.notifier());
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

async fn connect(args: &MountArgs) -> anyhow::Result<Arc<Space>> {
    let (authority, space, token_file) = (&args.authority, &args.space, &args.token_file);
    let remote = Remote::new(authority, space, token_file.clone(), &args.resolve)
        .with_context(|| format!("reading token file {}", token_file.display()))?;
    let root = remote.stat(SpacePath::root()).await;
    let root = root.with_context(|| format!("cannot reach space {space:?} at the authority {authority}"))?;
    let cache = Cache::open(args.cache.clone(), args.cache_bytes)
        .with_context(|| format!("opening cache {}", args.cache.display()))?;
    let (journal, woken) = Journal::open(args.journal.clone())
        .with_context(|| format!("opening journal {}", args.journal.display()))?;
    let (remote, cache, journal) = (Arc::new(remote), Arc::new(cache), Arc::new(journal));
    let stats = Arc::new(Stats::default());
    let committer = Committer {
        journal: journal.clone(),
        remote: remote.clone(),
        cache: cache.clone(),
        stats: stats.clone(),
    };
    tokio::spawn(Arc::new(committer).run(woken));
    tokio::spawn(remote.clone().probe());
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
    let space = Space::new(remote, cache, journal, stats, decider, root.id)?;
    Ok(Arc::new(space))
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

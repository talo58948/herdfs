use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use clap::Parser;
use herdfs_authority::{Authority, Policy, server::Server};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use tracing_subscriber::EnvFilter;

/// The herdfs authority. S3 keys come from `AWS_ACCESS_KEY_ID` and
/// `AWS_SECRET_ACCESS_KEY`.
#[derive(Parser)]
struct Args {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    db: PathBuf,
    /// Where the authority writes chunks.
    #[arg(long)]
    s3_endpoint: String,
    /// Where clients download chunks; presigned URLs point here.
    #[arg(long)]
    s3_public_endpoint: String,
    #[arg(long)]
    s3_bucket: String,
    #[arg(long)]
    s3_region: String,
    #[arg(long)]
    oidc_issuer: String,
    #[arg(long)]
    oidc_audience: String,
    /// The newest versions of each file kept, its head included.
    #[arg(long, default_value_t = 16)]
    retain_versions: u32,
    /// Versions younger than this many days are kept.
    #[arg(long, default_value_t = 30)]
    retain_days: u32,
    /// Seconds between collections.
    #[arg(long, default_value_t = 600)]
    gc_every: u64,
    /// Seconds an unreferenced chunk must age before it is deleted; at least twice the grant TTL.
    #[arg(long, default_value_t = 3600)]
    gc_grace: u64,
    /// Seconds a presigned chunk URL stays valid.
    #[arg(long, default_value_t = 60)]
    grant_ttl: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let args = Args::parse();
    let key = |name: &str| std::env::var(name).with_context(|| format!("{name} is not set"));
    let credentials = Credentials::new(key("AWS_ACCESS_KEY_ID")?, key("AWS_SECRET_ACCESS_KEY")?);
    let bucket = |endpoint: &str| -> anyhow::Result<Bucket> {
        let (name, region) = (args.s3_bucket.clone(), args.s3_region.clone());
        Ok(Bucket::new(endpoint.parse()?, UrlStyle::Path, name, region)?)
    };
    let (bucket, public_bucket) = (bucket(&args.s3_endpoint)?, bucket(&args.s3_public_endpoint)?);
    let authority = Authority::open(&args.db).with_context(|| format!("opening {}", args.db.display()))?;
    // Outstanding URLs and uploads awaiting their commit must not race the sweep.
    anyhow::ensure!(
        args.gc_grace >= 2 * args.grant_ttl,
        "--gc-grace must be at least twice --grant-ttl"
    );
    let policy = Policy {
        versions: args.retain_versions,
        max_age_ms: i64::from(args.retain_days) * 24 * 60 * 60 * 1000,
        grace_ms: i64::try_from(args.gc_grace)? * 1000,
    };
    let (issuer, audience) = (args.oidc_issuer, args.oidc_audience);
    let server = Server::new(
        authority,
        bucket,
        public_bucket,
        credentials,
        issuer,
        audience,
        Duration::from_secs(args.grant_ttl),
    );
    let server = Arc::new(server);
    tokio::spawn(Arc::clone(&server).collect_every(Duration::from_secs(args.gc_every), policy));
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(listen = %args.listen, "serving");
    axum::serve(listener, server.router()).await?;
    Ok(())
}

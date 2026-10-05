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
    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
    gc_every: u64,
    /// Seconds an unreferenced chunk must age; at least twice the grant TTL and interval plus TTL.
    #[arg(long, default_value_t = 3600)]
    gc_grace: u64,
    /// Seconds a presigned chunk URL stays valid.
    #[arg(long, default_value_t = 60)]
    grant_ttl: u64,
}

struct Settings {
    policy: Policy,
    collect_every: Duration,
    grant_ttl: Duration,
}

fn settings(args: &Args) -> anyhow::Result<Settings> {
    Ok(Settings {
        policy: Policy {
            versions: args.retain_versions,
            max_age_ms: i64::from(args.retain_days) * 24 * 60 * 60 * 1000,
            grace_ms: i64::try_from(args.gc_grace)? * 1000,
        },
        collect_every: Duration::from_secs(args.gc_every),
        grant_ttl: Duration::from_secs(args.grant_ttl),
    })
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
    // Outstanding URLs and uploads awaiting their commit must not race the sweep, whose grace
    // counts from the collection before a chunk left the live set.
    anyhow::ensure!(
        args.gc_grace >= 2 * args.grant_ttl && args.gc_grace >= args.gc_every + args.grant_ttl,
        "--gc-grace must be at least twice --grant-ttl, and at least --gc-every plus --grant-ttl"
    );
    let settings = settings(&args)?;
    let (issuer, audience) = (args.oidc_issuer, args.oidc_audience);
    let server = Server::new(
        authority,
        bucket,
        public_bucket,
        credentials,
        issuer,
        audience,
        settings.grant_ttl,
    );
    let server = Arc::new(server);
    tokio::spawn(Arc::clone(&server).collect_every(settings.collect_every, settings.policy));
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(listen = %args.listen, "serving");
    axum::serve(listener, server.router()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_convert_to_runtime_durations() {
        // contract: C14, C15, S4, S7
        let args = Args {
            listen: "127.0.0.1:0".parse().unwrap(),
            db: PathBuf::new(),
            s3_endpoint: String::new(),
            s3_public_endpoint: String::new(),
            s3_bucket: String::new(),
            s3_region: String::new(),
            oidc_issuer: String::new(),
            oidc_audience: String::new(),
            retain_versions: 7,
            retain_days: 2,
            gc_every: 3,
            gc_grace: 4,
            grant_ttl: 5,
        };

        let settings = settings(&args).unwrap();
        assert_eq!(settings.policy.versions, 7);
        assert_eq!(settings.policy.max_age_ms, 172_800_000);
        assert_eq!(settings.policy.grace_ms, 4_000);
        assert_eq!(settings.collect_every, Duration::from_secs(3));
        assert_eq!(settings.grant_ttl, Duration::from_secs(5));
    }

    #[test]
    fn zero_collection_interval_is_rejected() {
        // contract: S7
        let parsed = Args::try_parse_from([
            "authority",
            "--listen",
            "127.0.0.1:0",
            "--db",
            "db",
            "--s3-endpoint",
            "http://store",
            "--s3-public-endpoint",
            "http://store",
            "--s3-bucket",
            "bucket",
            "--s3-region",
            "region",
            "--oidc-issuer",
            "http://issuer",
            "--oidc-audience",
            "audience",
            "--gc-every",
            "0",
        ]);
        assert!(parsed.is_err());
    }
}

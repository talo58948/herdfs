use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use clap::{Parser, Subcommand};
use herdfs_authority::{Authority, LocalIssuer, Policy, server::Server};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use tracing_subscriber::EnvFilter;

/// The herdfs authority. S3 keys come from `AWS_ACCESS_KEY_ID` and
/// `AWS_SECRET_ACCESS_KEY`.
#[derive(Parser)]
#[command(subcommand_negates_reqs = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, required = true)]
    listen: Option<SocketAddr>,
    #[arg(long, required = true)]
    db: Option<PathBuf>,
    /// Where the authority writes chunks.
    #[arg(long, required = true)]
    s3_endpoint: Option<String>,
    /// Where clients download chunks; presigned URLs point here.
    #[arg(long, required = true)]
    s3_public_endpoint: Option<String>,
    #[arg(long, required = true)]
    s3_bucket: Option<String>,
    #[arg(long, required = true)]
    s3_region: Option<String>,
    #[arg(long, requires = "oidc_audience", conflicts_with = "local_issuer")]
    oidc_issuer: Option<String>,
    #[arg(long, requires = "oidc_issuer", conflicts_with = "local_issuer")]
    oidc_audience: Option<String>,
    /// Issues and validates tokens without an external identity provider.
    #[arg(long)]
    local_issuer: bool,
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

#[derive(Subcommand)]
enum Command {
    /// Prints a local-issuer token for one machine.
    IssueToken {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        user: String,
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
    /// Revokes a local-issuer token by its issued id.
    RevokeToken {
        #[arg(long)]
        db: PathBuf,
        id: String,
    },
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
    let mut args = Args::parse();
    if let Some(command) = args.command.take() {
        return run_command(command);
    }
    serve(args).await
}

fn run_command(command: Command) -> anyhow::Result<()> {
    match command {
        Command::IssueToken { db, user, days } => {
            let authority = Authority::open(&db)?;
            let issuer = LocalIssuer::load(&db)?;
            let ttl = Duration::from_secs(u64::from(days) * 24 * 60 * 60);
            let (id, token) = issuer.issue(&authority, &user, ttl)?;
            eprintln!("token id: {id}");
            println!("{token}");
        }
        Command::RevokeToken { db, id } => {
            let authority = Authority::open(&db)?;
            LocalIssuer::load(&db)?.revoke(&authority, &id)?;
        }
    }
    Ok(())
}

async fn serve(args: Args) -> anyhow::Result<()> {
    let requested = args.listen.context("--listen is required")?;
    let listener = tokio::net::TcpListener::bind(requested).await?;
    let listen = listener.local_addr()?;
    let db = args.db.as_ref().context("--db is required")?;
    let s3_endpoint = args.s3_endpoint.as_deref().context("--s3-endpoint is required")?;
    let s3_public_endpoint = args
        .s3_public_endpoint
        .as_deref()
        .context("--s3-public-endpoint is required")?;
    let s3_bucket = args.s3_bucket.as_ref().context("--s3-bucket is required")?;
    let s3_region = args.s3_region.as_ref().context("--s3-region is required")?;
    anyhow::ensure!(
        args.local_issuer || args.oidc_issuer.is_some(),
        "use --local-issuer or set --oidc-issuer and --oidc-audience"
    );
    let key = |name: &str| std::env::var(name).with_context(|| format!("{name} is not set"));
    let credentials = Credentials::new(key("AWS_ACCESS_KEY_ID")?, key("AWS_SECRET_ACCESS_KEY")?);
    let bucket = |endpoint: &str| -> anyhow::Result<Bucket> {
        let (name, region) = (s3_bucket.clone(), s3_region.clone());
        Ok(Bucket::new(endpoint.parse()?, UrlStyle::Path, name, region)?)
    };
    let (bucket, public_bucket) = (bucket(s3_endpoint)?, bucket(s3_public_endpoint)?);
    let authority = Authority::open(db).with_context(|| format!("opening {}", db.display()))?;
    // Outstanding URLs and uploads awaiting their commit must not race the sweep, whose grace
    // counts from the collection before a chunk left the live set.
    anyhow::ensure!(
        args.gc_grace >= 2 * args.grant_ttl && args.gc_grace >= args.gc_every + args.grant_ttl,
        "--gc-grace must be at least twice --grant-ttl, and at least --gc-every plus --grant-ttl"
    );
    let settings = settings(&args)?;
    let server = if args.local_issuer {
        let issuer = LocalIssuer::open(db, listen)?;
        Server::new_local(authority, bucket, public_bucket, credentials, issuer, settings.grant_ttl)
    } else {
        Server::new(
            authority,
            bucket,
            public_bucket,
            credentials,
            args.oidc_issuer.context("--oidc-issuer is required")?,
            args.oidc_audience.context("--oidc-audience is required")?,
            settings.grant_ttl,
        )
    };
    let server = Arc::new(server);
    tokio::spawn(Arc::clone(&server).collect_every(settings.collect_every, settings.policy));
    tracing::info!(%listen, "serving");
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
            command: None,
            listen: Some("127.0.0.1:0".parse().unwrap()),
            db: Some(PathBuf::new()),
            s3_endpoint: Some(String::new()),
            s3_public_endpoint: Some(String::new()),
            s3_bucket: Some(String::new()),
            s3_region: Some(String::new()),
            oidc_issuer: Some(String::new()),
            oidc_audience: Some(String::new()),
            local_issuer: false,
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
}

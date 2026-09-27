use std::{net::SocketAddr, path::PathBuf};

use anyhow::Context;
use clap::Parser;
use herdfs_authority::{Authority, server::Server};
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
    let (issuer, audience) = (args.oidc_issuer, args.oidc_audience);
    let server = Server::new(authority, bucket, public_bucket, credentials, issuer, audience);
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(listen = %args.listen, "serving");
    axum::serve(listener, server.router()).await?;
    Ok(())
}

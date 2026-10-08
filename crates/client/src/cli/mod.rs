//! Command-line parsing and dispatch for `herdfs`.

mod commands;
mod login;
mod output;

use std::io::{self, IsTerminal};
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use herdfs_core::proto::{Perm, VersionId};
use tracing_subscriber::EnvFilter;

use crate::MountArgs;

/// herdfs: a shared filesystem client.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Prints machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mounts a space and serves it until unmounted, SIGINT or SIGTERM.
    Mount(Box<MountArgs>),
    /// Prints the latest access plan and fetched bytes for a mounted file.
    Explain { path: PathBuf },
    /// Signs in through an OIDC issuer.
    Login(login::LoginArgs),
    /// Deletes the saved login tokens.
    Logout(TokenArgs),
    /// Prints the authenticated principal and groups.
    Whoami(ConnectArgs),
    /// Lists spaces reachable by the authenticated principal.
    Spaces(ConnectArgs),
    /// Lists visible versions, oldest first.
    History(PathArgs),
    /// Downloads a visible version.
    Show(ShowArgs),
    /// Restores a visible version as a new head.
    Restore(VersionArgs),
    /// Grants a principal access at a path.
    Share(ShareArgs),
    /// Removes a principal's explicit grant at a path.
    Revoke(PrincipalArgs),
    /// Lists explicit grants at a path and its ancestors.
    Access(PathArgs),
    /// Forks a space without copying chunk bytes.
    Fork(ForkArgs),
    /// Creates a space.
    CreateSpace(SpaceArgs),
    /// Deletes a space.
    DeleteSpace(SpaceArgs),
    /// Deletes every non-head version of a path.
    Purge(PathArgs),
}

#[derive(Clone, Args)]
struct TokenArgs {
    /// Login token file.
    #[arg(long, default_value_os_t = crate::token::default_path())]
    token_file: PathBuf,
}

#[derive(Clone, Args)]
struct ConnectArgs {
    /// Authority base URL.
    #[arg(long)]
    authority: String,
    #[command(flatten)]
    token: TokenArgs,
    /// Connect to IP for HOST while retaining the signed URL host.
    #[arg(long, value_parser = parse_resolve)]
    resolve: Vec<(String, IpAddr)>,
}

#[derive(Args)]
struct SpaceArgs {
    #[command(flatten)]
    connect: ConnectArgs,
    space: String,
}

#[derive(Args)]
struct PathArgs {
    #[command(flatten)]
    connect: ConnectArgs,
    space: String,
    path: String,
}

#[derive(Args)]
struct ShowArgs {
    #[command(flatten)]
    path: PathArgs,
    #[arg(long)]
    version: u64,
    /// Writes bytes to FILE instead of standard output.
    #[arg(short, long)]
    output: Option<PathBuf>,
}

#[derive(Args)]
struct VersionArgs {
    #[command(flatten)]
    path: PathArgs,
    #[arg(long)]
    version: u64,
}

#[derive(Clone, Copy, ValueEnum)]
enum SharePerm {
    Read,
    Write,
    Admin,
}

impl From<SharePerm> for Perm {
    fn from(value: SharePerm) -> Self {
        match value {
            SharePerm::Read => Self::Read,
            SharePerm::Write => Self::Write,
            SharePerm::Admin => Self::Admin,
        }
    }
}

#[derive(Args)]
struct PrincipalArgs {
    #[command(flatten)]
    path: PathArgs,
    principal: String,
}

#[derive(Args)]
struct ShareArgs {
    #[command(flatten)]
    principal: PrincipalArgs,
    perm: SharePerm,
}

#[derive(Args)]
struct ForkArgs {
    #[command(flatten)]
    connect: ConnectArgs,
    space: String,
    new_name: String,
}

pub fn run() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(io::stderr().is_terminal())
        .with_writer(io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Mount(args) => crate::mount(&args),
        Command::Explain { path } => explain(&path),
        Command::Logout(args) => crate::token::remove(&args.token_file).map_err(Into::into),
        Command::Login(args) => runtime()?.block_on(login::login(&args, cli.json)),
        command => runtime()?.block_on(commands::run(command, cli.json)),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Runtime::new()?)
}

fn explain(path: &PathBuf) -> anyhow::Result<()> {
    let value = xattr::get(path, "user.herdfs.plan")?
        .with_context(|| format!("{} has not been read since mount", path.display()))?;
    let json: serde_json::Value = serde_json::from_slice(&value)?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

fn version(id: u64) -> VersionId {
    VersionId(id)
}

fn parse_resolve(value: &str) -> Result<(String, IpAddr), String> {
    let (host, ip) = value.split_once('=').ok_or("expected HOST=IP")?;
    let ip = ip.parse().map_err(|_| format!("invalid IP address {ip:?}"))?;
    Ok((host.to_owned(), ip))
}

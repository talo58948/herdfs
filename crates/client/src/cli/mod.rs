//! Command-line parsing and dispatch for `herdfs`.

mod commands;
mod login;
mod output;

use std::io::{self, IsTerminal};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use herdfs_core::proto::Perm;
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
    /// Reports a running mount's connection, uploads, cache and kept paths.
    Status { mountpoint: Option<PathBuf> },
    /// Keeps a file or directory downloaded.
    Pin { path: PathBuf },
    /// Stops keeping a file or directory downloaded.
    Unpin { path: PathBuf },
    /// Stops starting new commits.
    Pause { mountpoint: Option<PathBuf> },
    /// Starts commits again.
    Resume { mountpoint: Option<PathBuf> },
    /// Reports Finder badge state for a path.
    File { path: PathBuf },
    /// Signs in through an OIDC issuer.
    Login(login::LoginArgs),
    /// Deletes the saved login tokens.
    Logout(TokenArgs),
    #[command(flatten)]
    Authority(AuthorityCommand),
}

#[derive(Subcommand)]
enum AuthorityCommand {
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

#[derive(Args)]
struct TokenArgs {
    /// Login token file.
    #[arg(long)]
    token_file: Option<PathBuf>,
}

impl TokenArgs {
    fn path(&self) -> anyhow::Result<PathBuf> {
        self.token_file
            .clone()
            .map_or_else(crate::token::default_path, Ok)
    }
}

#[derive(Args)]
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
    version: VersionArgs,
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
        Command::Explain { path } => control_path("explain", &path, cli.json),
        Command::Status { mountpoint } => control_mount("status", mountpoint.as_deref(), cli.json),
        Command::Pin { path } => control_path("pin", &path, cli.json),
        Command::Unpin { path } => control_path("unpin", &path, cli.json),
        Command::Pause { mountpoint } => control_mount("pause", mountpoint.as_deref(), cli.json),
        Command::Resume { mountpoint } => control_mount("resume", mountpoint.as_deref(), cli.json),
        Command::File { path } => control_path("file", &path, cli.json),
        Command::Logout(args) => crate::token::remove(&args.path()?).map_err(Into::into),
        Command::Login(args) => runtime()?.block_on(login::login(&args, cli.json)),
        Command::Authority(command) => runtime()?.block_on(commands::run(command, cli.json)),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Runtime::new()?)
}

fn control_path(op: &str, path: &Path, json: bool) -> anyhow::Result<()> {
    let registration = crate::control::find_mount(path)?;
    let path = crate::control::absolute(path)?;
    let request = serde_json::json!({"op":op,"path":path});
    control(registration, request, json)
}

fn control_mount(op: &str, mountpoint: Option<&Path>, json: bool) -> anyhow::Result<()> {
    let registration = crate::control::only_mount(mountpoint)?;
    control(registration, serde_json::json!({"op":op}), json)
}

fn control(
    registration: crate::control::Registration,
    request: serde_json::Value,
    json: bool,
) -> anyhow::Result<()> {
    let response = runtime()?.block_on(crate::control::request(&registration, request))?;
    if json {
        println!("{}", serde_json::to_string(&response)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&response)?);
    }
    anyhow::ensure!(response["ok"] == true, "{}", response["error"].as_str().unwrap_or("control request failed"));
    Ok(())
}

fn parse_resolve(value: &str) -> Result<(String, IpAddr), String> {
    let (host, ip) = value.split_once('=').ok_or("expected HOST=IP")?;
    let ip = ip.parse().map_err(|_| format!("invalid IP address {ip:?}"))?;
    Ok((host.to_owned(), ip))
}

//! The `herdfs` command.

use std::io::{self, IsTerminal};

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// herdfs: a shared filesystem client.
#[derive(Parser)]
#[command(version)]
enum Command {
    /// Mounts a space and serves it until unmounted, SIGINT or SIGTERM.
    Mount(Box<herdfs_client::MountArgs>),
    /// Prints the latest access plan and fetched bytes for a mounted file.
    Explain { path: PathBuf },
}

fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let ansi = io::stderr().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi)
        .with_writer(io::stderr)
        .init();
    match Command::parse() {
        Command::Mount(args) => herdfs_client::mount(&args),
        Command::Explain { path } => {
            let value = xattr::get(&path, "user.herdfs.plan")?
                .with_context(|| format!("{} has no completed access to explain", path.display()))?;
            let json: serde_json::Value = serde_json::from_slice(&value)?;
            println!("{}", serde_json::to_string_pretty(&json)?);
            Ok(())
        }
    }
}

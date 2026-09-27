//! The `herdfs` command.

use std::io::{self, IsTerminal};

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// herdfs: a shared filesystem client.
#[derive(Parser)]
#[command(version)]
enum Command {
    /// Mounts a space and serves it until unmounted, SIGINT or SIGTERM.
    Mount(herdfs_client::MountArgs),
}

fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let ansi = io::stderr().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi)
        .with_writer(io::stderr)
        .init();
    let Command::Mount(args) = Command::parse();
    herdfs_client::mount(&args)
}

//! The `herdfs` command.

fn main() -> anyhow::Result<()> {
    herdfs_client::cli::run()
}

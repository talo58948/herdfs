//! Local mount discovery and its newline-delimited JSON control channel (U1, U2).

mod registry;
mod server;

pub(crate) use registry::{Registration, find_mount, only_mount};
pub(crate) use server::serve;

use std::path::Path;

use anyhow::Context;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub(crate) async fn request(registration: &Registration, request: Value) -> anyhow::Result<Value> {
    let stream = UnixStream::connect(&registration.socket)
        .await
        .with_context(|| format!("connecting to mount at {}", registration.mountpoint.display()))?;
    let (read, mut write) = stream.into_split();
    write.write_all(&serde_json::to_vec(&request)?).await?;
    write.write_all(b"\n").await?;
    let mut lines = BufReader::new(read).lines();
    let line = lines.next_line().await?.context("mount closed its control socket")?;
    Ok(serde_json::from_str(&line)?)
}

pub(crate) fn absolute(path: &Path) -> anyhow::Result<std::path::PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

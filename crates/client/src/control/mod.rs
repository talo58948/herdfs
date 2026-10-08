//! Local mount discovery and its newline-delimited JSON control channel (U1, U2).

mod registry;
mod server;

pub(crate) use registry::{Registration, canonical_path, find_mount, only_mount};
pub(crate) use server::{Guard, serve};

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
    let line = lines
        .next_line()
        .await?
        .context("mount closed its control socket")?;
    Ok(serde_json::from_str(&line)?)
}

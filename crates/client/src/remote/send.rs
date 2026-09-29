//! Sending requests, refreshing authentication, and probing an offline link.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use reqwest::{RequestBuilder, StatusCode};

use super::Remote;
use crate::{Error, Result, lock};

pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a connection may stay silent before its request fails and can be retried.
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an authority call may wait for its answer to start, so a silent outage shows up
/// fast. The answer's body, such as a long chunk list on a slow link, may take longer as long
/// as it keeps coming (`READ_TIMEOUT`), and opens no offline window.
const RPC_TIMEOUT: Duration = Duration::from_secs(3);
/// A background call, a commit or a chunk upload, may wait this long for its answer to start,
/// plus a second per `SLOWEST_UPLOAD` bytes of its body, since its body goes first.
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(10);
const SLOWEST_UPLOAD: usize = 16 << 10;
/// How often the probe looks whether an offline window has ended.
const PROBE_EVERY: Duration = Duration::from_secs(1);

impl Remote {
    /// Stats the root when an offline window ends, and at most every `PROBE_EVERY` after, so the
    /// client finds out the authority answers again even while nothing else asks it.
    pub async fn probe(self: Arc<Self>) {
        loop {
            let left = self.link.remaining().unwrap_or_default();
            tokio::time::sleep(left.max(PROBE_EVERY)).await;
            if self.link.remaining() == Some(Duration::ZERO) {
                let _ = self.stat(herdfs_core::SpacePath::root()).await;
            }
        }
    }

    /// Sends unless the authority counts as unreachable, and records whether it answered in
    /// time: `RPC_TIMEOUT` for an interactive call, longer for a background one carrying `body`
    /// bytes, whose timeout is a slow upload rather than an outage and opens no offline window.
    pub(super) async fn send(
        &self,
        request: impl Fn(&str) -> RequestBuilder,
        body: Option<usize>,
    ) -> Result<reqwest::Response> {
        self.link.check()?;
        let upload = |len: usize| BACKGROUND_TIMEOUT + Duration::from_secs((len / SLOWEST_UPLOAD) as u64);
        let deadline = body.map_or(RPC_TIMEOUT, upload);
        let response = tokio::time::timeout(deadline, self.exchange(request)).await;
        let response = response.unwrap_or(Err(Error::TimedOut));
        if body.is_none() || !matches!(response, Err(Error::TimedOut | Error::Network(_))) {
            self.link.record(&response);
        }
        response
    }

    /// Sends with the current token. On 401 it re-reads the token file and tries once more,
    /// so a refreshed token takes effect without a remount.
    async fn exchange(&self, request: impl Fn(&str) -> RequestBuilder) -> Result<reqwest::Response> {
        let token = lock(&self.token).clone();
        let mut response = request(&token).send().await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let fresh = read_token(&self.token_file)?;
            response = request(&fresh).send().await?;
            *lock(&self.token) = fresh;
        }
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let refusal = serde_json::from_slice(&response.bytes().await?);
        Err(refusal.map_or_else(|_| Error::Protocol(format!("HTTP {status}")), Error::Api))
    }
}

pub(super) fn read_token(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

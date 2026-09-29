//! Typed calls to the authority, chunk uploads and verified chunk downloads.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{fs, io, time::Duration, time::Instant};

use bytes::Bytes;
use herdfs_core::proto::{Entry, EntryId, Request, Response, Version, VersionId};
use herdfs_core::{ChunkId, ChunkRef, SpacePath};
use reqwest::{RequestBuilder, StatusCode, header::CONTENT_TYPE};

use crate::{Average, Error, Result, journal::Record, lock};

mod link;

pub use link::Link;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a connection may stay silent before its request fails and can be retried.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an authority call may wait for its answer to start, so a silent outage shows up fast.
/// A body may take longer while it keeps coming (`READ_TIMEOUT`), and opens no offline window.
const RPC_TIMEOUT: Duration = Duration::from_secs(3);
/// A background call, a commit or a chunk upload, may wait this long for its answer to start,
/// plus a second per `SLOWEST_UPLOAD` bytes of its body, since its body goes first.
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(10);
const SLOWEST_UPLOAD: usize = 16 << 10;
const PROBE_EVERY: Duration = Duration::from_secs(1);

pub struct Remote {
    http: reqwest::Client,
    authority: String,
    space: String,
    token_file: PathBuf,
    token: Mutex<String>,
    /// Seconds from sending an authority call to its answer, over recent calls.
    pub round_trip: Average,
    pub link: Link,
}

impl Remote {
    /// `resolve` pins host names to addresses like curl's `--resolve`, so the lab can route
    /// presigned URLs through a proxy without breaking their signed host.
    pub fn new(
        authority: &str,
        space: &str,
        token_file: PathBuf,
        resolve: &[(String, IpAddr)],
    ) -> Result<Self> {
        let builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT);
        // reqwest ignores this port and keeps the URL's.
        let builder = resolve.iter().fold(builder, |builder, (host, ip)| {
            builder.resolve(host, SocketAddr::new(*ip, 0))
        });
        Ok(Self {
            http: builder.build()?,
            authority: authority.trim_end_matches('/').to_owned(),
            space: space.to_owned(),
            token: Mutex::new(read_token(&token_file)?),
            token_file,
            round_trip: Average::default(),
            link: Link::default(),
        })
    }

    pub async fn stat(&self, path: SpacePath) -> Result<Entry> {
        self.entry(|space| Request::Stat { space, path }).await
    }

    /// The entry with this id, at its current path.
    pub async fn locate(&self, entry: EntryId) -> Result<Entry> {
        self.entry(|space| Request::Locate { space, entry }).await
    }

    pub async fn lookup(&self, parent: EntryId, name: String) -> Result<Entry> {
        self.entry(|space| Request::Lookup { space, parent, name }).await
    }

    pub async fn list(&self, entry: EntryId) -> Result<Vec<Entry>> {
        match self.call(|space| Request::List { space, entry }).await? {
            Response::Entries { entries } => Ok(entries),
            other => Err(unexpected(&other)),
        }
    }

    /// The entry, its head version, and whether the caller may commit to it.
    pub async fn open(&self, entry: EntryId) -> Result<(Entry, Option<Version>, bool)> {
        match self.call(|space| Request::Open { space, entry }).await? {
            Response::Opened {
                entry,
                version,
                writable,
            } => Ok((entry, version, writable)),
            other => Err(unexpected(&other)),
        }
    }

    /// Presigned URLs for chunks of `version`, and how long they remain valid.
    pub async fn grant(
        &self,
        entry: EntryId,
        version: VersionId,
        chunks: Vec<u32>,
    ) -> Result<(Vec<String>, Duration)> {
        let request = |space| Request::Grant {
            space,
            entry,
            version,
            chunks,
        };
        match self.call(request).await? {
            Response::Grants { urls, ttl_ms } => Ok((urls, Duration::from_millis(ttl_ms))),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn create(&self, parent: EntryId, name: String, mode: u32) -> Result<Entry> {
        self.entry(|space| Request::Create {
            space,
            parent,
            name,
            mode,
        })
        .await
    }

    pub async fn mkdir(&self, parent: EntryId, name: String, mode: u32) -> Result<Entry> {
        self.entry(|space| Request::Mkdir {
            space,
            parent,
            name,
            mode,
        })
        .await
    }

    pub async fn symlink(&self, parent: EntryId, name: String, target: String) -> Result<Entry> {
        self.entry(|space| Request::Symlink {
            space,
            parent,
            name,
            target,
        })
        .await
    }

    pub async fn set_attr(&self, entry: EntryId, mode: Option<u32>, mtime_ms: Option<i64>) -> Result<Entry> {
        self.entry(|space| Request::SetAttr {
            space,
            entry,
            mode,
            mtime_ms,
        })
        .await
    }

    /// Removes the named entry within `parent`, and answers with it.
    pub async fn remove(&self, parent: EntryId, name: String) -> Result<Entry> {
        self.entry(|space| Request::Remove { space, parent, name }).await
    }

    pub async fn rename(
        &self,
        from_parent: EntryId,
        from_name: String,
        to_parent: EntryId,
        to_name: String,
    ) -> Result<(SpacePath, SpacePath)> {
        let request = |space| Request::Rename {
            space,
            from_parent,
            from_name,
            to_parent,
            to_name,
        };
        match self.call(request).await? {
            Response::Renamed { from, to } => Ok((from, to)),
            other => Err(unexpected(&other)),
        }
    }

    /// Makes `chunks` the entry's head. Unguarded, so the last commit wins (C5).
    pub async fn commit(&self, record: &Record, chunks: Vec<ChunkRef>) -> Result<Option<Version>> {
        let request = |space| Request::Commit {
            space,
            entry: record.entry,
            request_id: record.request_id.clone(),
            base: record.base.as_ref().map(|base| base.id),
            guarded: false,
            chunks,
            mtime_ms: record.mtime_ms,
        };
        match self.call(request).await? {
            Response::Version { version } => Ok(Some(version)),
            Response::Done => Ok(None),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn put_chunk(&self, request_id: &str, id: ChunkId, bytes: Bytes) -> Result<()> {
        let (authority, space) = (&self.authority, &self.space);
        let url = format!("{authority}/v1/spaces/{space}/commits/{request_id}/chunks/{id}");
        let upload = |token: &str| self.http.put(&url).bearer_auth(token).body(bytes.clone());
        self.send(upload, Some(bytes.len())).await.map(drop)
    }

    /// Fetches a chunk from a presigned URL and checks that it is the chunk expected.
    pub async fn download(&self, url: &str, expected: ChunkRef) -> Result<Vec<u8>> {
        let response = self.http.get(url).send().await?.error_for_status()?;
        let bytes = response.bytes().await?;
        if bytes.len() != expected.len as usize || ChunkId::of(&bytes) != expected.id {
            return Err(Error::Corrupt(expected.id));
        }
        Ok(bytes.into())
    }

    async fn entry(&self, request: impl FnOnce(String) -> Request) -> Result<Entry> {
        match self.call(request).await? {
            Response::Entry { entry } => Ok(entry),
            other => Err(unexpected(&other)),
        }
    }

    /// Sends the request that `request` builds for this client's space.
    async fn call(&self, request: impl FnOnce(String) -> Request) -> Result<Response> {
        let (url, request) = (format!("{}/v1/rpc", self.authority), request(self.space.clone()));
        // Serialized once, for every try and for the deadline of a commit, which runs in the
        // background with a chunk list that may be long.
        let body = Bytes::from(serde_json::to_vec(&request).map_err(io::Error::from)?);
        let background = matches!(request, Request::Commit { .. }).then_some(body.len());
        let rpc = |token: &str| {
            let json = self.http.post(&url).header(CONTENT_TYPE, "application/json");
            json.bearer_auth(token).body(body.clone())
        };
        let started = Instant::now();
        let response = self.send(rpc, background).await?;
        self.round_trip.add(started.elapsed().as_secs_f64());
        Ok(response.json().await?)
    }

    /// Stats the root when an offline window ends, and at most every `PROBE_EVERY` after, so the
    /// client finds out the authority answers again even while nothing else asks it.
    pub async fn probe(self: Arc<Self>) {
        loop {
            let left = self.link.remaining().unwrap_or_default();
            tokio::time::sleep(left.max(PROBE_EVERY)).await;
            if self.link.remaining() == Some(Duration::ZERO) {
                let _ = self.stat(SpacePath::root()).await;
            }
        }
    }

    /// Sends unless the authority counts as unreachable, and records whether it answered in
    /// time: `RPC_TIMEOUT` for an interactive call, longer for a background one carrying `body`
    /// bytes, whose timeout is a slow upload rather than an outage and opens no offline window.
    async fn send(
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

fn read_token(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

fn unexpected(response: &Response) -> Error {
    Error::Protocol(format!("{response:?}"))
}

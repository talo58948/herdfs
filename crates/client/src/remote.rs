//! Typed calls to the authority, chunk uploads and verified chunk downloads.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::{fs, sync::Mutex, time::Duration, time::Instant};

use bytes::Bytes;
use herdfs_core::proto::{self, Entry, Request, Response, Version, VersionId};
use herdfs_core::{ChunkId, ChunkRef, SpacePath};
use reqwest::{RequestBuilder, StatusCode};

use crate::{Average, error::Error, journal::Record, lock};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a connection may stay silent before its request fails and can be retried.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Remote {
    http: reqwest::Client,
    authority: String,
    space: String,
    token_file: PathBuf,
    token: Mutex<String>,
    /// Seconds from sending an authority call to its answer, over recent calls.
    pub round_trip: Average,
}

impl Remote {
    /// `resolve` pins host names to addresses like curl's `--resolve`, so the lab can route
    /// presigned URLs through a proxy without breaking their signed host.
    pub fn new(
        authority: &str,
        space: &str,
        token_file: PathBuf,
        resolve: &[(String, IpAddr)],
    ) -> Result<Self, Error> {
        let token = read_token(&token_file)?;
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
            token_file,
            token: Mutex::new(token),
            round_trip: Average::default(),
        })
    }

    pub async fn stat(&self, path: SpacePath) -> Result<Entry, Error> {
        self.entry(|space| Request::Stat { space, path }).await
    }

    pub async fn list(&self, path: SpacePath) -> Result<Vec<Entry>, Error> {
        match self.call(|space| Request::List { space, path }).await? {
            Response::Entries { entries } => Ok(entries),
            other => Err(unexpected(&other)),
        }
    }

    /// The entry at `path` and its head version, `None` until its first commit.
    pub async fn open(&self, path: SpacePath) -> Result<(Entry, Option<Version>), Error> {
        match self.call(|space| Request::Open { space, path }).await? {
            Response::Opened { entry, version } => Ok((entry, version)),
            other => Err(unexpected(&other)),
        }
    }

    /// Presigned URLs for chunks of `version`, in the order asked, and their expiry (Unix ms).
    pub async fn grant(
        &self,
        path: SpacePath,
        version: VersionId,
        chunks: Vec<u32>,
    ) -> Result<(Vec<String>, i64), Error> {
        let request = |space| Request::Grant {
            space,
            path,
            version,
            chunks,
        };
        match self.call(request).await? {
            Response::Grants { urls, expires_ms } => Ok((urls, expires_ms)),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn create(&self, path: SpacePath, mode: u32) -> Result<Entry, Error> {
        self.entry(|space| Request::Create { space, path, mode }).await
    }

    pub async fn mkdir(&self, path: SpacePath, mode: u32) -> Result<Entry, Error> {
        self.entry(|space| Request::Mkdir { space, path, mode }).await
    }

    pub async fn symlink(&self, path: SpacePath, target: String) -> Result<Entry, Error> {
        self.entry(|space| Request::Symlink { space, path, target }).await
    }

    pub async fn set_attr(
        &self,
        path: SpacePath,
        mode: Option<u32>,
        mtime_ms: Option<i64>,
    ) -> Result<Entry, Error> {
        self.entry(|space| Request::SetAttr {
            space,
            path,
            mode,
            mtime_ms,
        })
        .await
    }

    pub async fn remove(&self, path: SpacePath) -> Result<(), Error> {
        self.call(|space| Request::Remove { space, path }).await.map(drop)
    }

    pub async fn rename(&self, from: SpacePath, to: SpacePath) -> Result<(), Error> {
        self.call(|space| Request::Rename { space, from, to })
            .await
            .map(drop)
    }

    /// Makes `chunks` the entry's head. Unguarded, so the last commit wins (C5).
    pub async fn commit(&self, record: &Record, chunks: Vec<ChunkRef>) -> Result<Version, Error> {
        let Record {
            entry,
            base,
            request_id,
            ..
        } = record.clone();
        let request = |space| Request::Commit {
            space,
            entry,
            request_id,
            base,
            guarded: false,
            chunks,
        };
        match self.call(request).await? {
            Response::Version { version } => Ok(version),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn put_chunk(&self, id: ChunkId, bytes: Bytes) -> Result<(), Error> {
        let url = format!("{}/v1/spaces/{}/chunks/{id}", self.authority, self.space);
        let upload = |token: &str| self.http.put(&url).bearer_auth(token).body(bytes.clone());
        self.send(upload).await.map(drop)
    }

    /// Fetches a chunk from a presigned URL and checks that it is the chunk expected.
    pub async fn download(&self, url: &str, expected: ChunkRef) -> Result<Vec<u8>, Error> {
        let response = self.http.get(url).send().await?.error_for_status()?;
        let bytes = response.bytes().await?;
        if bytes.len() != expected.len as usize || ChunkId::of(&bytes) != expected.id {
            return Err(Error::Corrupt(expected.id));
        }
        Ok(bytes.into())
    }

    async fn entry(&self, request: impl FnOnce(String) -> Request) -> Result<Entry, Error> {
        match self.call(request).await? {
            Response::Entry { entry } => Ok(entry),
            other => Err(unexpected(&other)),
        }
    }

    /// Sends the request that `request` builds for this client's space.
    async fn call(&self, request: impl FnOnce(String) -> Request) -> Result<Response, Error> {
        let (url, request) = (format!("{}/v1/rpc", self.authority), request(self.space.clone()));
        let rpc = |token: &str| self.http.post(&url).bearer_auth(token).json(&request);
        let started = Instant::now();
        let response = self.send(rpc).await?;
        self.round_trip.add(started.elapsed().as_secs_f64());
        Ok(response.json().await?)
    }

    /// Sends with the current token. On 401 it re-reads the token file and tries once more,
    /// so a refreshed token takes effect without a remount.
    async fn send(&self, request: impl Fn(&str) -> RequestBuilder) -> Result<reqwest::Response, Error> {
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
        let body = response.bytes().await?;
        let failed = || Error::Protocol(format!("HTTP {status}"));
        Err(serde_json::from_slice::<proto::Error>(&body).map_or_else(|_| failed(), Error::Api))
    }
}

fn read_token(path: &Path) -> Result<String, Error> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

fn unexpected(response: &Response) -> Error {
    Error::Protocol(format!("{response:?}"))
}

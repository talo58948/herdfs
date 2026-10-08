//! Typed calls to the authority, chunk uploads and verified chunk downloads.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::{io, time::Duration, time::Instant};

use bytes::Bytes;
use herdfs_core::proto::{Entry, EntryId, Request, Response, Version, VersionId};
use herdfs_core::{ChunkId, ChunkRef, SpacePath};
use reqwest::header::CONTENT_TYPE;

use crate::{Average, Error, Result, journal::Record, token};

mod link;
mod send;

pub use link::Link;
use send::{CONNECT_TIMEOUT, READ_TIMEOUT};

pub struct Remote {
    http: reqwest::Client,
    authority: String,
    space: String,
    token: token::Source,
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
            token: token::Source::new(token_file)?,
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

    /// The entry, its head version (`None` until its first commit), and whether
    /// the caller may commit to it.
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

    /// Presigned URLs for chunks of `version` of `entry`, in the order asked, and how long
    /// they stay valid from when the call was sent.
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
    ) -> Result<(SpacePath, Entry)> {
        let request = |space| Request::Rename {
            space,
            from_parent,
            from_name,
            to_parent,
            to_name,
        };
        match self.call(request).await? {
            Response::Renamed { from, entry } => Ok((from, entry)),
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
        self.request(request(self.space.clone())).await
    }

    pub(crate) async fn request(&self, request: Request) -> Result<Response> {
        let url = format!("{}/v1/rpc", self.authority);
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
}

fn unexpected(response: &Response) -> Error {
    Error::Protocol(format!("{response:?}"))
}

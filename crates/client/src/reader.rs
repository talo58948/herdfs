//! Reads of remote versions through the verified chunk cache. Every strategy reads through
//! here, so all of them return the same bytes (C8).

use std::sync::{Arc, Mutex};
use std::{collections::HashMap, fs::File, io::Write, time::Instant, time::SystemTime};

use futures::stream::{self, BoxStream, StreamExt};
use herdfs_core::proto::{Version, VersionId};
use herdfs_core::{ChunkId, SpacePath};
use tokio::sync::{Mutex as FetchLock, OwnedMutexGuard};

use crate::cache::{Cache, Lru};
use crate::stats::{Stats, bump};
use crate::{Average, chunk_map::ChunkMap, error::Error, lock, remote::Remote, unix_ms};

/// Concurrent downloads per request, which is also the prefetch batch size.
const DOWNLOADS: usize = 8;
/// A cached grant this close to expiry is renewed rather than reused.
const GRANT_MARGIN_MS: i64 = 10_000;
/// Verified chunks kept in memory, so the FUSE reads within one chunk do not each re-read
/// and re-hash it from disk.
const MEMORY_BYTES: u64 = 64 << 20;

/// Chunk bytes by id, as one read gathers them.
type Found = HashMap<ChunkId, Arc<Vec<u8>>>;
/// The bytes of fetched chunks, in the order asked, each as soon as it arrives.
type Fetched<'a> = Result<BoxStream<'a, Result<Vec<u8>, Error>>, Error>;

/// A version opened for reading.
pub struct OpenVersion {
    pub version: Version,
    map: ChunkMap,
}

impl OpenVersion {
    pub fn new(version: Version) -> Self {
        let map = ChunkMap::new(&version.chunks);
        Self { version, map }
    }

    fn id(&self, index: usize) -> ChunkId {
        self.version.chunks[index].id
    }
}

pub struct Reader {
    remote: Arc<Remote>,
    cache: Arc<Cache>,
    stats: Arc<Stats>,
    memory: Mutex<Lru<Arc<Vec<u8>>>>,
    /// Presigned URLs by version and chunk index, with their expiry, reused until then.
    grants: Mutex<HashMap<(VersionId, usize), (String, i64)>>,
    /// A lock per chunk being fetched, so concurrent reads download it once.
    fetching: Mutex<HashMap<ChunkId, Arc<FetchLock<()>>>>,
    /// Download throughput in bytes per second, over recent chunk downloads.
    pub throughput: Average,
}

impl Reader {
    pub fn new(remote: Arc<Remote>, cache: Arc<Cache>, stats: Arc<Stats>) -> Self {
        let (memory, grants, fetching) = (Mutex::default(), Mutex::default(), Mutex::default());
        Self {
            remote,
            cache,
            stats,
            memory,
            grants,
            fetching,
            throughput: Average::default(),
        }
    }

    /// Bytes `[off, off + len)` of `file`, fetching only the chunks that overlap them (C9),
    /// with grants asked under `path`.
    pub async fn read(
        &self,
        file: &OpenVersion,
        path: &SpacePath,
        off: u64,
        len: u64,
    ) -> Result<Vec<u8>, Error> {
        self.read_with(file, off, len, |missing| self.fetch(file, path, missing))
            .await
    }

    /// Downloads every chunk not yet cached, for the Fetch and Pin strategies.
    pub async fn prefetch(&self, file: &OpenVersion, path: &SpacePath) -> Result<(), Error> {
        let fetch = |missing| self.fetch(file, path, missing);
        let chunks = 0..file.version.chunks.len();
        let missing: Vec<usize> = chunks
            .filter(|&index| !self.cache.contains(&file.id(index)))
            .collect();
        for batch in missing.chunks(DOWNLOADS) {
            self.load(file, batch, fetch).await?;
        }
        Ok(())
    }

    /// Writes all of `file` to `out`, to start a journal copy from it.
    pub async fn materialize(
        &self,
        file: &OpenVersion,
        path: &SpacePath,
        out: &mut File,
    ) -> Result<(), Error> {
        let fetch = |missing| self.fetch(file, path, missing);
        let indices: Vec<usize> = (0..file.version.chunks.len()).collect();
        for batch in indices.chunks(DOWNLOADS) {
            let chunks = self.load(file, batch, fetch).await?;
            chunks.iter().try_for_each(|chunk| out.write_all(chunk))?;
        }
        Ok(())
    }

    async fn read_with<'a>(
        &self,
        file: &OpenVersion,
        off: u64,
        len: u64,
        fetch: impl AsyncFnOnce(Vec<usize>) -> Fetched<'a>,
    ) -> Result<Vec<u8>, Error> {
        let indices: Vec<usize> = file.map.overlapping(off, len).collect();
        let chunks = self.load(file, &indices, fetch).await?;
        let mut bytes = Vec::new();
        for (index, chunk) in indices.into_iter().zip(chunks) {
            let part = chunk.get(file.map.within(index, off, len));
            bytes.extend_from_slice(part.ok_or(Error::Corrupt(file.id(index)))?);
        }
        Ok(bytes)
    }

    /// The chunks at `indices`, from memory or the disk cache where they are and from
    /// `fetch` otherwise.
    async fn load<'a>(
        &self,
        file: &OpenVersion,
        indices: &[usize],
        fetch: impl AsyncFnOnce(Vec<usize>) -> Fetched<'a>,
    ) -> Result<Vec<Arc<Vec<u8>>>, Error> {
        let mut found = Found::new();
        let missing = self.look_up(file, indices, &mut found);
        if !missing.is_empty() {
            let mut claims = self.claim(file, &missing).await;
            // Another read may have fetched them while this one waited for its claims.
            let needed = self.look_up(file, &missing, &mut found);
            let mut fetched = fetch(needed.clone()).await?;
            for index in needed {
                let fewer = || Error::Protocol("fetched fewer chunks than asked".into());
                let (id, bytes) = (file.id(index), Arc::new(fetched.next().await.ok_or_else(fewer)??));
                if let Err(err) = self.cache.put(&id, &bytes) {
                    tracing::warn!(%err, "could not cache a chunk");
                }
                self.keep(id, bytes.clone());
                found.insert(id, bytes);
                // A read waiting for this chunk goes on without waiting for the rest.
                claims.remove(&id);
            }
            drop(claims);
            lock(&self.fetching).retain(|_, fetch| Arc::strong_count(fetch) > 1);
        }
        let chunk = |index: &usize| {
            found
                .get(&file.id(*index))
                .cloned()
                .ok_or(Error::Corrupt(file.id(*index)))
        };
        indices.iter().map(chunk).collect()
    }

    /// Adds the chunks at `indices` that memory or the disk cache has to `found`, and returns
    /// the others, once per chunk id.
    fn look_up(&self, file: &OpenVersion, indices: &[usize], found: &mut Found) -> Vec<usize> {
        let mut missing: Vec<usize> = Vec::new();
        for &index in indices {
            let id = file.id(index);
            if found.contains_key(&id) || missing.iter().any(|&other| file.id(other) == id) {
                continue;
            }
            match self.cached(&id) {
                Some(bytes) => {
                    bump(&self.stats.cache_hits, 1);
                    found.insert(id, bytes);
                }
                None => missing.push(index),
            }
        }
        missing
    }

    /// A verified chunk from memory, or from the disk cache, which checks its hash (C11).
    fn cached(&self, id: &ChunkId) -> Option<Arc<Vec<u8>>> {
        if let Some(bytes) = lock(&self.memory).get(id) {
            return Some(bytes);
        }
        let bytes = Arc::new(self.cache.get(id)?);
        self.keep(*id, bytes.clone());
        Some(bytes)
    }

    fn keep(&self, id: ChunkId, bytes: Arc<Vec<u8>>) {
        let mut memory = lock(&self.memory);
        memory.insert(id, bytes.len() as u64, bytes);
        memory.evict(MEMORY_BYTES, |_| false);
    }

    /// Takes the fetch lock of each chunk, in id order so that claims cannot deadlock.
    async fn claim(&self, file: &OpenVersion, indices: &[usize]) -> HashMap<ChunkId, OwnedMutexGuard<()>> {
        let mut ids: Vec<ChunkId> = indices.iter().map(|&index| file.id(index)).collect();
        ids.sort();
        ids.dedup();
        let locks: Vec<_> = {
            let mut fetching = lock(&self.fetching);
            ids.iter()
                .map(|id| fetching.entry(*id).or_default().clone())
                .collect()
        };
        let mut claims = HashMap::new();
        for (id, fetch) in ids.into_iter().zip(locks) {
            claims.insert(id, fetch.lock_owned().await);
        }
        claims
    }

    /// Grants chunks of `file`, then downloads them, verifying each against its id.
    async fn fetch<'a>(
        &'a self,
        file: &'a OpenVersion,
        path: &SpacePath,
        indices: Vec<usize>,
    ) -> Fetched<'a> {
        let urls = self.urls(file, path, &indices).await?;
        let downloads = indices.into_iter().zip(urls);
        let downloads = downloads.map(|(index, url)| self.download(file, index, url));
        Ok(stream::iter(downloads).buffered(DOWNLOADS).boxed())
    }

    async fn download(&self, file: &OpenVersion, index: usize, url: String) -> Result<Vec<u8>, Error> {
        let started = Instant::now();
        let chunk = self.remote.download(&url, file.version.chunks[index]).await;
        let Ok(bytes) = &chunk else {
            lock(&self.grants).remove(&(file.version.id, index));
            return chunk;
        };
        bump(&self.stats.chunks_fetched, 1);
        bump(&self.stats.bytes_fetched, bytes.len() as u64);
        self.throughput
            .add(bytes.len() as f64 / started.elapsed().as_secs_f64());
        chunk
    }

    /// URLs for chunks `indices`, reusing unexpired grants and asking for the rest in one call.
    async fn urls(
        &self,
        file: &OpenVersion,
        path: &SpacePath,
        indices: &[usize],
    ) -> Result<Vec<String>, Error> {
        let (version, usable_after) = (file.version.id, unix_ms(SystemTime::now()) + GRANT_MARGIN_MS);
        let usable = |index: &usize| {
            let grants = lock(&self.grants);
            let (url, expires_ms) = grants.get(&(version, *index))?;
            (*expires_ms > usable_after).then(|| url.clone())
        };
        let wanted: Vec<u32> = indices
            .iter()
            .filter(|index| usable(index).is_none())
            .map(|&i| i as u32)
            .collect();
        if !wanted.is_empty() {
            let (urls, expires_ms) = self.remote.grant(path.clone(), version, wanted.clone()).await?;
            bump(&self.stats.grants, 1);
            let mut grants = lock(&self.grants);
            grants.retain(|_, (_, expires)| *expires > usable_after);
            let granted = wanted.into_iter().zip(urls);
            grants.extend(granted.map(|(index, url)| ((version, index as usize), (url, expires_ms))));
        }
        let missing = || Error::Protocol("grant lacks usable URLs".into());
        indices
            .iter()
            .map(|index| usable(index).ok_or_else(missing))
            .collect()
    }
}

#[cfg(test)]
mod tests;

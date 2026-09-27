//! Reads of remote versions through the verified chunk cache. Every strategy reads through
//! here, so all of them return the same bytes (C8).

use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use futures::{StreamExt, TryStreamExt, stream};
use herdfs_core::{
    ChunkId, SpacePath,
    proto::{Version, VersionId},
};
use tokio::sync::{Mutex as FetchLock, OwnedMutexGuard};

use crate::{
    cache::Cache,
    chunk_map::ChunkMap,
    error::Error,
    lock,
    remote::Remote,
    stats::{Stats, bump},
    unix_ms,
};

/// Concurrent downloads per request, which is also the prefetch batch size.
const DOWNLOADS: usize = 8;
/// A cached grant this close to expiry is renewed rather than reused.
const GRANT_MARGIN_MS: i64 = 10_000;

/// A version opened for reading, with the path its grants are asked under.
pub struct OpenVersion {
    pub path: SpacePath,
    pub version: Version,
    map: ChunkMap,
}

impl OpenVersion {
    pub fn new(path: SpacePath, version: Version) -> Self {
        let map = ChunkMap::new(&version.chunks);
        Self { path, version, map }
    }

    fn id(&self, index: usize) -> ChunkId {
        self.version.chunks[index].id
    }
}

pub struct Reader {
    remote: Arc<Remote>,
    cache: Arc<Cache>,
    stats: Arc<Stats>,
    /// Presigned URLs by version and chunk index, with their expiry, reused until then.
    grants: Mutex<HashMap<(VersionId, usize), (String, i64)>>,
    /// A lock per chunk being fetched, so concurrent reads download it once.
    fetching: Mutex<HashMap<ChunkId, Arc<FetchLock<()>>>>,
}

impl Reader {
    pub fn new(remote: Arc<Remote>, cache: Arc<Cache>, stats: Arc<Stats>) -> Self {
        Self {
            remote,
            cache,
            stats,
            grants: Mutex::default(),
            fetching: Mutex::default(),
        }
    }

    /// Bytes `[off, off + len)` of `file`, fetching only the chunks that overlap them (C9).
    pub async fn read(&self, file: &OpenVersion, off: u64, len: u64) -> Result<Vec<u8>, Error> {
        self.read_with(file, off, len, |missing| self.fetch(file, missing))
            .await
    }

    /// Downloads every chunk not yet cached, for the Fetch and Pin strategies.
    pub async fn prefetch(&self, file: &OpenVersion) -> Result<(), Error> {
        let missing: Vec<usize> = (0..file.version.chunks.len())
            .filter(|&index| !self.cache.contains(&file.id(index)))
            .collect();
        for batch in missing.chunks(DOWNLOADS) {
            self.load(file, batch, |missing| self.fetch(file, missing))
                .await?;
        }
        Ok(())
    }

    /// Writes all of `file` to `out`, to start a journal copy from it.
    pub async fn materialize(&self, file: &OpenVersion, out: &mut File) -> Result<(), Error> {
        let indices: Vec<usize> = (0..file.version.chunks.len()).collect();
        for batch in indices.chunks(DOWNLOADS) {
            for chunk in self
                .load(file, batch, |missing| self.fetch(file, missing))
                .await?
            {
                out.write_all(&chunk)?;
            }
        }
        Ok(())
    }

    async fn read_with<F, Fut>(
        &self,
        file: &OpenVersion,
        off: u64,
        len: u64,
        fetch: F,
    ) -> Result<Vec<u8>, Error>
    where
        F: FnOnce(Vec<usize>) -> Fut,
        Fut: Future<Output = Result<Vec<Vec<u8>>, Error>>,
    {
        let indices: Vec<usize> = file.map.overlapping(off, len).collect();
        let chunks = self.load(file, &indices, fetch).await?;
        let mut bytes = Vec::new();
        for (index, chunk) in indices.into_iter().zip(chunks) {
            let part = chunk.get(file.map.within(index, off, len));
            bytes.extend_from_slice(part.ok_or(Error::Corrupt(file.id(index)))?);
        }
        Ok(bytes)
    }

    /// The chunks at `indices`, from the cache where it has them and from `fetch` otherwise.
    async fn load<F, Fut>(
        &self,
        file: &OpenVersion,
        indices: &[usize],
        fetch: F,
    ) -> Result<Vec<Arc<Vec<u8>>>, Error>
    where
        F: FnOnce(Vec<usize>) -> Fut,
        Fut: Future<Output = Result<Vec<Vec<u8>>, Error>>,
    {
        let mut found = HashMap::new();
        let mut missing = Vec::new();
        self.look_up(file, indices, &mut found, &mut missing);
        if !missing.is_empty() {
            let claims = self.claim(file, &missing).await;
            let mut needed = Vec::new();
            // Another read may have fetched them while this one waited for its claims.
            self.look_up(file, &missing, &mut found, &mut needed);
            let fetched = fetch(needed.clone()).await?;
            if fetched.len() != needed.len() {
                return Err(Error::Protocol("fetched a different number of chunks".into()));
            }
            for (index, bytes) in needed.into_iter().zip(fetched) {
                if let Err(err) = self.cache.put(&file.id(index), &bytes) {
                    tracing::warn!(%err, "could not cache a chunk");
                }
                found.insert(file.id(index), Arc::new(bytes));
            }
            drop(claims);
            lock(&self.fetching).retain(|_, fetch| Arc::strong_count(fetch) > 1);
        }
        let chunk = |index: &usize| found.get(&file.id(*index)).cloned();
        indices
            .iter()
            .map(|index| chunk(index).ok_or(Error::Corrupt(file.id(*index))))
            .collect()
    }

    /// Sorts `indices` into chunks the cache has and chunks still `missing`, once per chunk id.
    fn look_up(
        &self,
        file: &OpenVersion,
        indices: &[usize],
        found: &mut HashMap<ChunkId, Arc<Vec<u8>>>,
        missing: &mut Vec<usize>,
    ) {
        for &index in indices {
            let id = file.id(index);
            if found.contains_key(&id) || missing.iter().any(|&other| file.id(other) == id) {
                continue;
            }
            match self.cache.get(&id) {
                Some(bytes) => {
                    bump(&self.stats.cache_hits, 1);
                    found.insert(id, Arc::new(bytes));
                }
                None => missing.push(index),
            }
        }
    }

    /// Takes the fetch lock of each chunk, in id order so that claims cannot deadlock.
    async fn claim(&self, file: &OpenVersion, indices: &[usize]) -> Vec<OwnedMutexGuard<()>> {
        let mut ids: Vec<ChunkId> = indices.iter().map(|&index| file.id(index)).collect();
        ids.sort();
        ids.dedup();
        let locks: Vec<_> = {
            let mut fetching = lock(&self.fetching);
            ids.iter()
                .map(|id| fetching.entry(*id).or_default().clone())
                .collect()
        };
        let mut claims = Vec::with_capacity(locks.len());
        for fetch in locks {
            claims.push(fetch.lock_owned().await);
        }
        claims
    }

    /// Grants and downloads chunks of `file`, verifying each against its id.
    async fn fetch(&self, file: &OpenVersion, indices: Vec<usize>) -> Result<Vec<Vec<u8>>, Error> {
        let urls = self.urls(file, &indices).await?;
        let downloads = indices.into_iter().zip(urls).map(|(index, url)| async move {
            let chunk = self.remote.download(&url, file.version.chunks[index]).await;
            if chunk.is_err() {
                lock(&self.grants).remove(&(file.version.id, index));
            }
            chunk
        });
        let chunks: Vec<Vec<u8>> = stream::iter(downloads).buffered(DOWNLOADS).try_collect().await?;
        bump(&self.stats.chunks_fetched, chunks.len() as u64);
        bump(
            &self.stats.bytes_fetched,
            chunks.iter().map(|chunk| chunk.len() as u64).sum(),
        );
        Ok(chunks)
    }

    /// URLs for chunks `indices`, reusing unexpired grants and asking for the rest in one call.
    async fn urls(&self, file: &OpenVersion, indices: &[usize]) -> Result<Vec<String>, Error> {
        let version = file.version.id;
        let reusable_after = unix_ms(SystemTime::now()) + GRANT_MARGIN_MS;
        let mut urls: HashMap<usize, String> = {
            let grants = lock(&self.grants);
            let reusable = |index: usize| {
                let (url, expires_ms) = grants.get(&(version, index))?;
                (*expires_ms > reusable_after).then(|| (index, url.clone()))
            };
            indices.iter().filter_map(|&index| reusable(index)).collect()
        };
        let wanted: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|index| !urls.contains_key(index))
            .collect();
        if !wanted.is_empty() {
            let chunks = wanted.iter().map(|&index| index as u32).collect();
            let (granted, expires_ms) = self.remote.grant(&file.path, version, chunks).await?;
            bump(&self.stats.grants, 1);
            let mut grants = lock(&self.grants);
            grants.retain(|_, (_, expires)| *expires > reusable_after);
            for (index, url) in wanted.into_iter().zip(granted) {
                grants.insert((version, index), (url.clone(), expires_ms));
                urls.insert(index, url);
            }
        }
        let url = |index| {
            urls.remove(index)
                .ok_or_else(|| Error::Protocol("grant lacks URLs".into()))
        };
        indices.iter().map(url).collect()
    }
}

#[cfg(test)]
mod tests;

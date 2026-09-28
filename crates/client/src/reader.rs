//! Reads of remote versions through the verified chunk cache. Every strategy reads through
//! here, so all of them return the same bytes (C8).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fs::File, io::Write};

use futures::stream::{self, BoxStream, StreamExt};
use herdfs_core::ChunkId;
use herdfs_core::proto::{EntryId, Version, VersionId};
use tokio::sync::{Mutex as FetchLock, OwnedMutexGuard};

use crate::stats::{Stats, bump};
use crate::version::OpenVersion;
use crate::{Average, Error, Result, cache::Cache, lock, lru::Lru, remote::Remote};

/// Concurrent downloads per request, which is also the prefetch batch size.
const DOWNLOADS: usize = 8;
/// A cached grant this close to expiry is renewed rather than reused; a shorter grant TTL
/// halves it.
const GRANT_MARGIN: Duration = Duration::from_secs(10);
/// Verified chunks kept in memory, so the FUSE reads within one chunk do not each re-read
/// and re-hash it from disk.
const MEMORY_BYTES: u64 = 64 << 20;
/// Entries whose current version the reader keeps.
const HEADS: u64 = 4096;

/// Chunk bytes by id, as one read gathers them.
type Found = HashMap<ChunkId, Arc<Vec<u8>>>;
/// The bytes of fetched chunks, in the order asked, each as soon as it arrives.
type Fetched<'a> = Result<BoxStream<'a, Result<Vec<u8>>>>;

/// The version an entry's open files read: `None` until its first commit.
pub type Head = Option<Arc<OpenVersion>>;

pub struct Reader {
    remote: Arc<Remote>,
    cache: Arc<Cache>,
    stats: Arc<Stats>,
    memory: Mutex<Lru<ChunkId, Arc<Vec<u8>>>>,
    /// Presigned URLs by version and chunk index, with when to renew them, reused until then.
    grants: Mutex<HashMap<(VersionId, usize), (String, Instant)>>,
    /// A lock per chunk being fetched, so concurrent reads download it once.
    fetching: Mutex<HashMap<ChunkId, Arc<FetchLock<()>>>>,
    /// Download throughput in bytes per second, over recent chunk downloads.
    pub throughput: Average,
    /// Each entry's newest head seen, which its open files read, and whether this client may
    /// write it. Offline, opens start from it (C12).
    heads: Mutex<Lru<EntryId, (Head, bool)>>,
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
            heads: Mutex::default(),
        }
    }

    /// The version the entry's open files read, and whether this client may write it, if known.
    pub fn known(&self, entry: EntryId) -> Option<(Head, bool)> {
        lock(&self.heads).get(&entry)
    }

    pub fn current(&self, entry: EntryId) -> Option<Head> {
        self.known(entry).map(|(file, _)| file)
    }

    /// Forgets the version the entry's open files read, which this client may no longer use.
    pub fn forget(&self, entry: EntryId) {
        lock(&self.heads).remove(&entry);
    }

    /// Makes `head` the version the entry's open files read, unless a newer one is known, and
    /// records whether this client may write it. Keeps the entries `open` names whatever the
    /// bound. True if that replaced a version known before, whose pages the kernel may hold.
    pub fn follow(
        &self,
        entry: EntryId,
        (head, writable): (Option<Version>, bool),
        open: impl Fn(&EntryId) -> bool,
    ) -> bool {
        let mut heads = lock(&self.heads);
        let known = heads.get(&entry).map(|(file, _)| file);
        let older =
            |file: &Head| file.as_ref().map(|file| file.version.id) < head.as_ref().map(|head| head.id);
        let switched = known.as_ref().is_some_and(older);
        let file = match known {
            Some(file) if !older(&file) => file,
            _ => head.map(|head| Arc::new(OpenVersion::new(head))),
        };
        heads.insert(entry, 1, (file, writable));
        heads.evict(HEADS, open);
        switched
    }

    /// Bytes `[off, off + len)` of `file`, fetching only the chunks that overlap them (C9).
    pub async fn read(&self, file: &OpenVersion, off: u64, len: u64) -> Result<Vec<u8>> {
        self.read_with(file, off, len, |missing| self.fetch(file, missing))
            .await
    }

    /// Downloads every chunk not yet cached, for the Fetch and Pin strategies.
    pub async fn prefetch(&self, file: &OpenVersion) -> Result<()> {
        let fetch = |missing| self.fetch(file, missing);
        let chunks = 0..file.version.chunks.len();
        let missing: Vec<usize> = chunks.filter(|&i| !self.cache.contains(&file.id(i))).collect();
        for batch in missing.chunks(DOWNLOADS) {
            self.load(file, batch, fetch).await?;
        }
        Ok(())
    }

    /// Writes all of `file` to `out`, to start a journal copy from it.
    pub async fn materialize(&self, file: &OpenVersion, out: &mut File) -> Result<()> {
        let fetch = |missing| self.fetch(file, missing);
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
    ) -> Result<Vec<u8>> {
        let indices: Vec<usize> = file.overlapping(off, len).collect();
        let chunks = self.load(file, &indices, fetch).await?;
        let mut bytes = Vec::new();
        for (index, chunk) in indices.into_iter().zip(chunks) {
            let part = chunk.get(file.within(index, off, len));
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
    ) -> Result<Vec<Arc<Vec<u8>>>> {
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
        let chunk = |&i: &usize| found.get(&file.id(i)).cloned().ok_or(Error::Corrupt(file.id(i)));
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
        // Never more than the disk cache may hold, so `--cache-bytes` bounds both.
        memory.evict(MEMORY_BYTES.min(self.cache.capacity()), |_| false);
    }

    /// Takes the fetch lock of each chunk, in id order so that claims cannot deadlock.
    async fn claim(&self, file: &OpenVersion, indices: &[usize]) -> HashMap<ChunkId, OwnedMutexGuard<()>> {
        let ids: BTreeSet<ChunkId> = indices.iter().map(|&index| file.id(index)).collect();
        let mut claims = HashMap::new();
        for id in ids {
            let fetch = lock(&self.fetching).entry(id).or_default().clone();
            claims.insert(id, fetch.lock_owned().await);
        }
        claims
    }

    /// Grants chunks of `file`, then downloads them, verifying each against its id.
    async fn fetch<'a>(&'a self, file: &'a OpenVersion, wanted: Vec<usize>) -> Fetched<'a> {
        let urls = self.urls(file, &wanted).await?;
        let downloads = wanted.into_iter().zip(urls);
        let downloads = downloads.map(|(index, url)| self.download(file, index, url));
        Ok(stream::iter(downloads).buffered(DOWNLOADS).boxed())
    }

    async fn download(&self, file: &OpenVersion, index: usize, url: String) -> Result<Vec<u8>> {
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
    /// Expiry is measured on this client's monotonic clock, so its wall clock may be off.
    async fn urls(&self, file: &OpenVersion, indices: &[usize]) -> Result<Vec<String>> {
        let version = file.version.id;
        let usable = |index: &usize| {
            let grants = lock(&self.grants);
            let (url, renew) = grants.get(&(version, *index))?;
            (*renew > Instant::now()).then(|| url.clone())
        };
        let wanted = indices.iter().filter(|i| usable(i).is_none());
        let wanted: Vec<u32> = wanted.map(|&i| i as u32).collect();
        if !wanted.is_empty() {
            let asked = Instant::now();
            let grant = self.remote.grant(file.version.entry, version, wanted.clone());
            let (urls, ttl) = grant.await?;
            bump(&self.stats.grants, 1);
            let mut grants = lock(&self.grants);
            grants.retain(|_, (_, renew)| *renew > Instant::now());
            let renew = asked + ttl - GRANT_MARGIN.min(ttl / 2);
            let granted = wanted.into_iter().zip(urls);
            grants.extend(granted.map(|(index, url)| ((version, index as usize), (url, renew))));
        }
        let missing = || Error::Protocol("grant lacks usable URLs".into());
        indices.iter().map(|i| usable(i).ok_or_else(missing)).collect()
    }
}

#[cfg(test)]
mod tests;

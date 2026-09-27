//! The on-disk chunk cache. Files are named by chunk id and verified on every read, so the
//! cache never serves bytes other than the chunk asked for (C11).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::{fs, io, path::PathBuf};

use herdfs_core::{ChunkId, ChunkRef, chunk::total_len};

use crate::lock;

pub struct Cache {
    dir: PathBuf,
    capacity: u64,
    index: Mutex<Index>,
}

#[derive(Default)]
struct Index {
    chunks: Lru<()>,
    /// Open counts of chunks that must not be evicted.
    pins: HashMap<ChunkId, usize>,
}

/// Chunks in order of last use, each with its length and a `T`.
#[derive(Default)]
pub struct Lru<T> {
    chunks: HashMap<ChunkId, (u64, u64, T)>,
    by_use: BTreeMap<u64, ChunkId>,
    bytes: u64,
    clock: u64,
}

impl<T: Clone> Lru<T> {
    pub fn insert(&mut self, id: ChunkId, len: u64, value: T) {
        self.remove(&id);
        self.clock += 1;
        self.chunks.insert(id, (len, self.clock, value));
        self.by_use.insert(self.clock, id);
        self.bytes += len;
    }

    /// The chunk's value, marking it used.
    pub fn get(&mut self, id: &ChunkId) -> Option<T> {
        let (len, _, value) = self.chunks.get(id)?.clone();
        self.insert(*id, len, value.clone());
        Some(value)
    }

    pub fn remove(&mut self, id: &ChunkId) {
        if let Some((len, used, _)) = self.chunks.remove(id) {
            self.by_use.remove(&used);
            self.bytes -= len;
        }
    }

    fn contains(&self, id: &ChunkId) -> bool {
        self.chunks.contains_key(id)
    }

    /// Drops the least recently used chunks, except those `keep` names, until the total
    /// fits `capacity`. Returns the dropped ids.
    pub fn evict(&mut self, capacity: u64, keep: impl Fn(&ChunkId) -> bool) -> Vec<ChunkId> {
        let mut excess = self.bytes.saturating_sub(capacity);
        let mut dropped = Vec::new();
        for id in self.by_use.values().filter(|id| !keep(id)) {
            if excess == 0 {
                break;
            }
            excess = excess.saturating_sub(self.chunks.get(id).map_or(0, |chunk| chunk.0));
            dropped.push(*id);
        }
        dropped.iter().for_each(|id| self.remove(id));
        dropped
    }
}

impl Cache {
    /// Opens the cache in `dir`, indexing the chunks an earlier run left there.
    pub fn open(dir: PathBuf, capacity: u64) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let mut found = Vec::new();
        for item in fs::read_dir(&dir)? {
            let item = item?;
            let name = item.file_name();
            if name.to_string_lossy().ends_with(".tmp") {
                fs::remove_file(item.path())?;
            } else if let Some(id) = name.to_str().and_then(|name| name.parse::<ChunkId>().ok()) {
                let meta = item.metadata()?;
                found.push((meta.modified()?, id, meta.len()));
            }
        }
        found.sort();
        let mut index = Index::default();
        for (_, id, len) in found {
            index.chunks.insert(id, len, ());
        }
        let index = Mutex::new(index);
        let cache = Self { dir, capacity, index };
        cache.evict();
        Ok(cache)
    }

    /// The chunk's bytes if they are cached and intact. A chunk that fails its hash is
    /// deleted, so the caller refetches it.
    pub fn get(&self, id: &ChunkId) -> Option<Vec<u8>> {
        let path = self.path(id);
        let Ok(bytes) = fs::read(&path) else {
            lock(&self.index).chunks.remove(id);
            return None;
        };
        if ChunkId::of(&bytes) != *id {
            tracing::warn!(%id, "cached chunk failed verification; refetching");
            lock(&self.index).chunks.remove(id);
            let _ = fs::remove_file(&path);
            return None;
        }
        lock(&self.index).chunks.insert(*id, bytes.len() as u64, ());
        Some(bytes)
    }

    /// Stores a verified chunk. The temp file and rename mean a crash never leaves a torn one.
    pub fn put(&self, id: &ChunkId, bytes: &[u8]) -> io::Result<()> {
        let temp = self.dir.join(format!("{id}.{:x}.tmp", rand::random::<u64>()));
        fs::write(&temp, bytes)?;
        fs::rename(&temp, self.path(id))?;
        lock(&self.index).chunks.insert(*id, bytes.len() as u64, ());
        self.evict();
        Ok(())
    }

    pub fn contains(&self, id: &ChunkId) -> bool {
        lock(&self.index).chunks.contains(id)
    }

    /// The fraction of `chunks`' bytes that are cached.
    pub fn coverage(&self, chunks: &[ChunkRef]) -> f64 {
        let total = total_len(chunks);
        if total == 0 {
            return 1.0;
        }
        let index = lock(&self.index);
        let cached: u64 = chunks
            .iter()
            .filter(|chunk| index.chunks.contains(&chunk.id))
            .map(|chunk| u64::from(chunk.len))
            .sum();
        cached as f64 / total as f64
    }

    /// Keeps `ids` through eviction until they are unpinned as often as pinned.
    pub fn pin(&self, ids: &[ChunkId]) {
        let mut index = lock(&self.index);
        for id in ids {
            *index.pins.entry(*id).or_default() += 1;
        }
    }

    pub fn unpin(&self, ids: &[ChunkId]) {
        let mut index = lock(&self.index);
        for id in ids {
            let pins = index.pins.entry(*id).or_default();
            *pins = pins.saturating_sub(1);
            if *pins == 0 {
                index.pins.remove(id);
            }
        }
        drop(index);
        self.evict();
    }

    fn evict(&self) {
        let Index { chunks, pins } = &mut *lock(&self.index);
        for id in chunks.evict(self.capacity, |id| pins.contains_key(id)) {
            let _ = fs::remove_file(self.path(&id));
        }
    }

    fn path(&self, id: &ChunkId) -> PathBuf {
        self.dir.join(id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(fill: u8) -> (ChunkId, Vec<u8>) {
        let bytes = vec![fill; 100];
        (ChunkId::of(&bytes), bytes)
    }

    // contract: C11
    #[test]
    fn a_tampered_chunk_is_a_miss_and_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().into(), 1 << 20).unwrap();
        let (id, bytes) = chunk(1);
        cache.put(&id, &bytes).unwrap();
        assert_eq!(cache.get(&id), Some(bytes));

        fs::write(dir.path().join(id.to_string()), vec![2; 100]).unwrap();
        assert_eq!(cache.get(&id), None);
        assert!(!dir.path().join(id.to_string()).exists());
        assert!(!cache.contains(&id));
    }

    // contract: C10
    #[test]
    fn eviction_drops_the_least_recently_used_first() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().into(), 300).unwrap();
        let [a, b, c, d] = [1, 2, 3, 4].map(chunk);
        for (id, bytes) in [&a, &b, &c] {
            cache.put(id, bytes).unwrap();
        }
        cache.get(&a.0).unwrap();
        cache.put(&d.0, &d.1).unwrap();
        assert!(!cache.contains(&b.0));
        assert!([&a, &c, &d].iter().all(|(id, _)| cache.get(id).is_some()));
    }

    // contract: C10
    #[test]
    fn pinned_chunks_survive_eviction_until_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().into(), 150).unwrap();
        let (pinned, bytes) = chunk(1);
        cache.pin(&[pinned]);
        cache.put(&pinned, &bytes).unwrap();
        for fill in 2..6 {
            let (id, bytes) = chunk(fill);
            cache.put(&id, &bytes).unwrap();
        }
        assert!(cache.get(&pinned).is_some());
        cache.unpin(&[pinned]);
        let (id, bytes) = chunk(9);
        cache.put(&id, &bytes).unwrap();
        assert!(!cache.contains(&pinned));
    }

    // contract: C10
    #[test]
    fn the_index_is_rebuilt_from_disk_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let (id, bytes) = chunk(1);
        Cache::open(dir.path().into(), 1 << 20)
            .unwrap()
            .put(&id, &bytes)
            .unwrap();
        fs::write(dir.path().join("leftover.0.tmp"), b"x").unwrap();

        let cache = Cache::open(dir.path().into(), 1 << 20).unwrap();
        assert!(cache.contains(&id));
        assert!(!dir.path().join("leftover.0.tmp").exists());
        assert_eq!(cache.coverage(&[ChunkRef { id, len: 100 }]), 1.0);
        drop(cache);
        assert!(!Cache::open(dir.path().into(), 50).unwrap().contains(&id));
    }
}

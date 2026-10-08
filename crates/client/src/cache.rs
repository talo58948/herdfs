//! The on-disk chunk cache. Files are named by chunk id and verified on every read, so the
//! cache never serves bytes other than the chunk asked for (C11).

use std::collections::HashMap;
use std::sync::Mutex;
use std::{fs, io, io::Write, path::PathBuf};

use herdfs_core::{ChunkId, ChunkRef, chunk::total_len};

use crate::{lock, lru::Lru};

pub struct Cache {
    dir: PathBuf,
    capacity: u64,
    index: Mutex<Index>,
}

#[derive(Default)]
struct Index {
    chunks: Lru<ChunkId, ()>,
    /// Open counts of chunks that must not be evicted.
    pins: HashMap<ChunkId, usize>,
}

impl Cache {
    /// Opens the cache in `dir`, indexing the chunks an earlier run left there.
    pub fn open(dir: PathBuf, capacity: u64) -> io::Result<Self> {
        Self::open_pinned(dir, capacity, &[])
    }

    /// Opens the cache while protecting durable pins before startup eviction runs.
    pub(crate) fn open_pinned(dir: PathBuf, capacity: u64, pinned: &[ChunkId]) -> io::Result<Self> {
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
        for id in pinned {
            *index.pins.entry(*id).or_default() += 1;
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
            let mut index = lock(&self.index);
            // A concurrent fetch may have put the right bytes there since; they stay.
            if fs::read(&path).is_ok_and(|now| ChunkId::of(&now) != *id) {
                index.chunks.remove(id);
                let _ = fs::remove_file(&path);
            }
            return None;
        }
        lock(&self.index).chunks.insert(*id, bytes.len() as u64, ());
        Some(bytes)
    }

    /// Stores a verified chunk. The temp file and rename mean a crash never leaves a torn one.
    pub fn put(&self, id: &ChunkId, bytes: &[u8]) -> io::Result<()> {
        let mut temp = tempfile::Builder::new().suffix(".tmp").tempfile_in(&self.dir)?;
        temp.write_all(bytes)?;
        let mut index = lock(&self.index);
        temp.persist(self.path(id)).map_err(|error| error.error)?;
        index.chunks.insert(*id, bytes.len() as u64, ());
        drop(index);
        self.evict();
        Ok(())
    }

    /// Failure is only logged because the verified bytes are in hand and caching is optional.
    pub(crate) fn put_best_effort(&self, id: &ChunkId, bytes: &[u8]) {
        if let Err(err) = self.put(id, bytes) {
            tracing::warn!(%err, "could not cache a chunk");
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    pub(crate) fn used(&self) -> u64 {
        lock(&self.index).chunks.total()
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
        let cached = chunks.iter().filter(|chunk| index.chunks.contains(&chunk.id));
        let cached: u64 = cached.map(|chunk| u64::from(chunk.len)).sum();
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

    // contract: C11
    #[test]
    fn a_failed_persist_removes_its_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().into(), 1 << 20).unwrap();
        let (id, bytes) = chunk(1);
        fs::create_dir(dir.path().join(id.to_string())).unwrap();

        assert!(cache.put(&id, &bytes).is_err());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(names.iter().all(|name| !name.to_string_lossy().ends_with(".tmp")));
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

    // contract: P4, C10
    #[test]
    fn durable_pins_are_protected_before_startup_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let (kept, bytes) = chunk(1);
        let (other, other_bytes) = chunk(2);
        let cache = Cache::open(dir.path().into(), 1 << 20).unwrap();
        cache.put(&kept, &bytes).unwrap();
        cache.put(&other, &other_bytes).unwrap();
        drop(cache);

        let cache = Cache::open_pinned(dir.path().into(), 50, &[kept]).unwrap();
        assert!(cache.contains(&kept));
        assert!(!cache.contains(&other));
        assert!(cache.used() > cache.capacity());
    }
}

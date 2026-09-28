//! A map that forgets its least recently used entries once they outgrow a capacity, each
//! weighing its length: bytes for chunks, one for metadata.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

pub struct Lru<K, V> {
    entries: HashMap<K, (u64, u64, V)>,
    by_use: BTreeMap<u64, K>,
    total: u64,
    /// The last use stamp, counting up from the middle to leave room below for cold inserts.
    clock: u64,
}

impl<K, V> Default for Lru<K, V> {
    fn default() -> Self {
        let (entries, by_use) = (HashMap::new(), BTreeMap::new());
        Self {
            entries,
            by_use,
            total: 0,
            clock: 1 << 63,
        }
    }
}

impl<K: Clone + Eq + Hash, V: Clone> Lru<K, V> {
    pub fn insert(&mut self, key: K, len: u64, value: V) {
        self.clock += 1;
        self.put(key, len, value, self.clock);
    }

    /// Inserts the key as the next to go unless it is in use, so entries fetched in bulk do not
    /// push out those in use.
    pub fn insert_cold(&mut self, key: K, len: u64, value: V) {
        let first = self.by_use.keys().next().map_or(self.clock, |first| first - 1);
        let used = self.entries.get(&key).map_or(first, |entry| entry.1);
        self.put(key, len, value, used);
    }

    fn put(&mut self, key: K, len: u64, value: V, used: u64) {
        self.remove(&key);
        self.by_use.insert(used, key.clone());
        self.entries.insert(key, (len, used, value));
        self.total += len;
    }

    /// The key's value, marking it used.
    pub fn get(&mut self, key: &K) -> Option<V> {
        let (len, _, value) = self.entries.get(key)?.clone();
        self.insert(key.clone(), len, value.clone());
        Some(value)
    }

    pub fn remove(&mut self, key: &K) {
        if let Some((len, used, _)) = self.entries.remove(key) {
            self.by_use.remove(&used);
            self.total -= len;
        }
    }

    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Drops the entries `stale` names.
    pub fn purge(&mut self, stale: impl Fn(&K) -> bool) {
        let keys: Vec<K> = self.entries.keys().filter(|key| stale(key)).cloned().collect();
        keys.iter().for_each(|key| self.remove(key));
    }

    /// Drops the least recently used entries, except those `keep` names, until the total fits
    /// `capacity`. Returns the dropped keys.
    pub fn evict(&mut self, capacity: u64, keep: impl Fn(&K) -> bool) -> Vec<K> {
        let mut excess = self.total.saturating_sub(capacity);
        let mut dropped = Vec::new();
        for key in self.by_use.values().filter(|key| !keep(key)) {
            if excess == 0 {
                break;
            }
            excess = excess.saturating_sub(self.entries.get(key).map_or(0, |entry| entry.0));
            dropped.push(key.clone());
        }
        dropped.iter().for_each(|key| self.remove(key));
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::Lru;

    // contract: C12
    #[test]
    fn entries_fetched_in_bulk_go_before_those_in_use() {
        let mut lru = Lru::default();
        lru.insert("used", 1, ());
        ["a", "b", "c"]
            .into_iter()
            .for_each(|key| lru.insert_cold(key, 1, ()));
        lru.get(&"a");
        lru.evict(2, |_| false);
        assert!(
            lru.contains(&"used") && lru.contains(&"a"),
            "the entries in use stay"
        );
        assert!(!lru.contains(&"b") && !lru.contains(&"c"));
    }
}

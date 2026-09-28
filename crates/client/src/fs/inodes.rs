//! The entries this client knows: each one's path, bound one to one with its id under one lock,
//! so that no decision about an entry is taken from a path another entry now holds (C7); the
//! entry as the authority last described it (C1, C2); and the kernel's references to its inode.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId};

use super::Asked;
use crate::{lock, lru::Lru};

/// How many entries that nothing holds are kept, for lookups within TTL and while offline (C12).
const IDLE: u64 = 4096;

#[derive(Default)]
pub struct Inodes(Mutex<Table>);

#[derive(Default)]
struct Table {
    nodes: HashMap<EntryId, Node>,
    by_path: HashMap<SpacePath, EntryId>,
    /// The nodes that nothing holds, the next to go first.
    idle: Lru<EntryId, ()>,
}

struct Node {
    /// The entry's path, and when the authority confirmed it: `None` once another entry took it.
    bound: Option<(SpacePath, Instant)>,
    /// References the kernel holds, which `forget` lowers.
    lookups: u64,
    inode: Arc<Inode>,
}

/// What this client knows of one entry.
pub struct Inode(Mutex<State>);

struct State {
    entry: Entry,
    /// When the entry was asked for, and in which generation of settled commits.
    asked: Asked,
    /// Whether it may answer for its path within TTL: a change this client made says not.
    current: bool,
}

impl Inodes {
    /// Takes an answer of the authority asked for at `asked`: the entry's attributes, unless ones
    /// asked for later are held, and its path, unless the authority confirmed a binding of the
    /// entry or the path since (C7). A listing's entries (`cold`) go first unless used (C12).
    pub fn observe(&self, entry: &Entry, asked: Asked, cold: bool) {
        lock(&self.0).observe(entry, asked, cold);
    }

    /// As `observe`, for an answer that hands the kernel a reference to the entry's inode.
    pub fn looked_up(&self, entry: &Entry, asked: Asked) {
        let mut table = lock(&self.0);
        table.observe(entry, asked, false);
        table.idle.remove(&entry.id);
        if let Some(node) = table.nodes.get_mut(&entry.id) {
            node.lookups += 1;
        }
    }

    /// Drops `lookups` of the kernel's references to the entry's inode.
    pub fn forget(&self, id: EntryId, lookups: u64) {
        let mut table = lock(&self.0);
        let Some(node) = table.nodes.get_mut(&id) else {
            return;
        };
        node.lookups = node.lookups.saturating_sub(lookups);
        if node.lookups == 0 {
            table.idle.insert(id, 1, ());
            table.evict();
        }
    }

    /// The entry bound to `path`, when it was asked for, and whether it may still answer for
    /// the path, marking it used.
    pub fn at(&self, path: &SpacePath) -> Option<(Entry, Asked, bool)> {
        let mut table = lock(&self.0);
        let id = *table.by_path.get(path)?;
        let _ = table.idle.get(&id);
        let state = lock(&table.nodes.get(&id)?.inode.0);
        Some((state.entry.clone(), state.asked, state.current))
    }

    /// Whether the entry is known, and its path with when it was confirmed, if it has one.
    pub fn bound(&self, id: EntryId) -> Option<Option<(SpacePath, Instant)>> {
        lock(&self.0).nodes.get(&id).map(|node| node.bound.clone())
    }

    /// The entries whose paths lie at or below `path`.
    pub fn within(&self, path: &SpacePath) -> Vec<EntryId> {
        let table = lock(&self.0);
        let at = |(id, node): (&EntryId, &Node)| node.bound.as_ref()?.0.is_within(path).then_some(*id);
        table.nodes.iter().filter_map(at).collect()
    }

    /// Unbinds `path`, where the authority no longer finds the entry bound to it (C7).
    pub fn unbind(&self, path: &SpacePath) {
        let mut table = lock(&self.0);
        if let Some(id) = table.by_path.remove(path)
            && let Some(node) = table.nodes.get_mut(&id)
        {
            node.bound = None;
        }
    }

    /// Forgets an entry this client removed.
    pub fn removed(&self, id: EntryId) {
        let mut table = lock(&self.0);
        if let Some((path, _)) = table.nodes.remove(&id).and_then(|node| node.bound) {
            table.by_path.remove(&path);
        }
        table.idle.remove(&id);
    }

    /// Moves every path at or below `from` to `to`, after this client's rename; an entry `to`
    /// named before loses its path, which `Locate` finds if it lives on elsewhere (C7).
    pub fn moved(&self, from: &SpacePath, to: &SpacePath) {
        let mut table = lock(&self.0);
        let table = &mut *table;
        if let Some(replaced) = table.by_path.remove(to).and_then(|id| table.nodes.get_mut(&id)) {
            replaced.bound = None;
        }
        let mut rebound = Vec::new();
        for (id, node) in &mut table.nodes {
            if let Some((path, _)) = &mut node.bound
                && let Some(moved) = path.rebase(from, to)
            {
                table.by_path.remove(path);
                rebound.push((moved.clone(), *id));
                *path = moved;
            }
        }
        table.by_path.extend(rebound);
    }

    /// Marks the entries at or below `path`, which this client just added, moved or removed, and
    /// its directory's, whose mtime moved with it, as not to answer for their paths.
    pub fn stale(&self, path: &SpacePath) {
        let (table, parent) = (lock(&self.0), path.parent());
        let changed = |known: &SpacePath| known.is_within(path) || Some(known) == parent.as_ref();
        for node in table.nodes.values() {
            if node.bound.as_ref().is_some_and(|(known, _)| changed(known)) {
                lock(&node.inode.0).current = false;
            }
        }
    }
}

impl Table {
    fn observe(&mut self, entry: &Entry, asked: Asked, cold: bool) {
        let state = || State {
            entry: entry.clone(),
            asked,
            current: true,
        };
        let node = self.nodes.entry(entry.id).or_insert_with(|| Node {
            bound: None,
            lookups: 0,
            inode: Arc::new(Inode(Mutex::new(state()))),
        });
        let (idle, bound, mut known) = (node.lookups == 0, node.bound.is_some(), lock(&node.inode.0));
        if asked.0 >= known.asked.0 {
            *known = state();
        }
        drop(known);
        // A listing names entries in bulk: it binds new ones, but confirms no binding (C7).
        if !(cold && bound) {
            self.bind(entry.id, &entry.path, asked.0);
        }
        match (idle, cold) {
            (false, _) => {}
            (true, true) => self.idle.insert_cold(entry.id, 1, ()),
            (true, false) => self.idle.insert(entry.id, 1, ()),
        }
        self.evict();
    }

    /// Binds `path` to `id` as confirmed `at`, unless a binding of either was confirmed later.
    fn bind(&mut self, id: EntryId, path: &SpacePath, at: Instant) {
        let later = |node: Option<&Node>| {
            let bound = node.and_then(|node| node.bound.as_ref());
            bound.is_some_and(|(_, confirmed)| *confirmed >= at)
        };
        let holder = self.by_path.get(path).copied().filter(|holder| *holder != id);
        if later(self.nodes.get(&id)) || later(holder.and_then(|holder| self.nodes.get(&holder))) {
            return;
        }
        if let Some(holder) = holder.and_then(|holder| self.nodes.get_mut(&holder)) {
            holder.bound = None;
        }
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        if let Some((old, _)) = node.bound.replace((path.clone(), at))
            && old != *path
        {
            self.by_path.remove(&old);
        }
        self.by_path.insert(path.clone(), id);
    }

    /// Drops the least recently used entries that nothing holds, past `IDLE`.
    fn evict(&mut self) {
        for id in self.idle.evict(IDLE, |_| false) {
            if let Some((path, _)) = self.nodes.remove(&id).and_then(|node| node.bound) {
                self.by_path.remove(&path);
            }
        }
    }
}

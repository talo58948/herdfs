//! The entries this client knows: each one's path, bound one to one with its id under one lock,
//! so that no decision about an entry is taken from a path another entry now holds (C7), and
//! the kernel's references to its inode. What is known of each entry is its `Inode`.

use std::collections::HashMap;
use std::fs::File;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId};

use super::inode::{Inode, State};
use crate::{journal::Local, lock, lru::Lru};

/// How many entries that nothing holds are kept, for lookups within TTL and while offline (C12).
const IDLE: u64 = 4096;

#[derive(Default)]
pub struct Inodes(Mutex<Table>);

#[derive(Default)]
struct Table {
    nodes: HashMap<EntryId, Node>,
    by_path: HashMap<SpacePath, EntryId>,
    /// The nodes the kernel holds no reference to, the next to go first; those with local
    /// state stay.
    idle: Lru<EntryId, ()>,
}

struct Node {
    /// The entry's path, and when the authority confirmed it: `None` once another entry took it.
    bound: Option<(SpacePath, Instant)>,
    /// References the kernel holds, which `forget` lowers.
    lookups: u64,
    inode: Arc<Inode>,
}

impl Inodes {
    /// Takes an answer of the authority asked for at `asked`: the entry's attributes, unless ones
    /// asked for later are held, and its path, unless the authority confirmed a binding of the
    /// entry or the path since (C7). A listing's entries (`cold`) go first unless used (C12).
    pub fn observe(&self, entry: &Entry, asked: Instant, cold: bool) {
        lock(&self.0).observe(entry, asked, cold);
    }

    /// As `observe`, for an answer that hands the kernel a reference to the entry's inode.
    pub fn looked_up(&self, entry: &Entry, asked: Instant) {
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
    pub fn at(&self, path: &SpacePath) -> Option<(Entry, Instant, bool)> {
        let mut table = lock(&self.0);
        let id = *table.by_path.get(path)?;
        let _ = table.idle.get(&id);
        let inode = table.nodes.get(&id)?.inode.clone();
        drop(table);
        inode.state().cached()
    }

    /// Reads or changes what is known of the entry, under its inode's lock alone.
    pub fn with<T>(&self, id: EntryId, change: impl FnOnce(&mut State) -> T) -> Option<T> {
        let inode = lock(&self.0).nodes.get(&id)?.inode.clone();
        Some(change(&mut inode.state()))
    }

    /// The entry as its inode now has it, with the journal copy of this client's changes, if
    /// any: one snapshot for a reply (C1). An inode forgotten meanwhile had no local state.
    pub fn view(&self, entry: Entry) -> (Entry, Option<Arc<File>>) {
        let view = self.with(entry.id, |state| state.view()).flatten();
        view.unwrap_or((entry, None))
    }

    /// As `with`, for the journal: with `make`, the entry first gets local state, and an inode if
    /// unknown, since a write recovered from the journal names only its entry.
    pub fn local<T>(&self, id: EntryId, make: bool, change: impl FnOnce(&mut State) -> T) -> Option<T> {
        let inode = {
            let mut table = lock(&self.0);
            let inode = match table.nodes.get(&id) {
                Some(node) => node.inode.clone(),
                None if make => table.add(id, None, Instant::now()),
                None => return None,
            };
            if make {
                inode.state().local.get_or_insert_with(Local::default);
            }
            inode
        };
        Some(change(&mut inode.state()))
    }

    /// How many entries have local state for which `counts` holds.
    pub fn count(&self, counts: impl Fn(&Local) -> bool) -> usize {
        let table = lock(&self.0);
        let local = |node: &&Node| node.inode.state().local.as_ref().is_some_and(&counts);
        table.nodes.values().filter(local).count()
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

    /// Forgets an entry this client removed, whose local state goes with it (C10).
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
                node.inode.state().current = false;
            }
        }
    }
}

impl Table {
    fn add(&mut self, id: EntryId, entry: Option<Entry>, asked: Instant) -> Arc<Inode> {
        let inode = Arc::new(Inode::new(entry, asked));
        let node = Node {
            bound: None,
            lookups: 0,
            inode: inode.clone(),
        };
        self.nodes.insert(id, node);
        self.idle.insert(id, 1, ());
        inode
    }

    fn observe(&mut self, entry: &Entry, asked: Instant, cold: bool) {
        let (bound, inode) = match self.nodes.get(&entry.id) {
            Some(node) => (node.bound.is_some(), node.inode.clone()),
            None => (false, self.add(entry.id, Some(entry.clone()), asked)),
        };
        inode.state().observe(entry, asked);
        // A listing names entries in bulk: it binds new ones, but confirms no binding (C7).
        if !(cold && bound) {
            self.bind(entry.id, &entry.path, asked);
        }
        if self.idle.contains(&entry.id) {
            match cold {
                true => self.idle.insert_cold(entry.id, 1, ()),
                false => self.idle.insert(entry.id, 1, ()),
            }
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
        let nodes = &self.nodes;
        let used = |id: &EntryId| nodes.get(id).is_some_and(|node| node.inode.in_use());
        for id in self.idle.evict(IDLE, used) {
            if let Some((path, _)) = self.nodes.remove(&id).and_then(|node| node.bound) {
                self.by_path.remove(&path);
            }
        }
    }
}

//! The path bindings behind `Inodes`: which entry each known path names, when the authority
//! confirmed it, and the rule that a later confirmation wins, which rebasing moved subtrees also
//! follows (C7).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId, Kind};

use super::IDLE;
use crate::fs::inode::Inode;
use crate::lru::Lru;

#[derive(Default)]
pub(super) struct Table {
    pub(super) nodes: HashMap<EntryId, Node>,
    pub(super) by_path: HashMap<SpacePath, EntryId>,
    /// The nodes the kernel holds no reference to, the next to go first; those with local
    /// state stay.
    pub(super) idle: Lru<EntryId, ()>,
}

pub(super) struct Node {
    /// The entry's path, and when the authority confirmed it: `None` once another entry took it.
    pub(super) bound: Option<(SpacePath, Instant)>,
    /// The path and time at which it became unbound.
    pub(super) last_path: Option<(SpacePath, Instant)>,
    /// References the kernel holds, which `forget` lowers.
    pub(super) lookups: u64,
    pub(super) inode: Arc<Inode>,
}

impl Node {
    pub(super) fn source(&self, at: Instant) -> Option<(SpacePath, Instant)> {
        let bound = self.bound.as_ref().map(|(path, _)| (path.clone(), at));
        bound.or_else(|| self.last_path.clone())
    }

    pub(super) fn unbind(&mut self, at: Instant) {
        self.last_path = self.bound.take().map(|(path, _)| (path, at));
    }
}

impl Table {
    pub(super) fn add(&mut self, id: EntryId, entry: Option<Entry>, asked: Instant) -> Arc<Inode> {
        let inode = Arc::new(Inode::new(entry, asked));
        let node = Node {
            bound: None,
            last_path: None,
            lookups: 0,
            inode: inode.clone(),
        };
        self.nodes.insert(id, node);
        self.idle.insert(id, 1, ());
        inode
    }

    pub(super) fn observe(&mut self, entry: &Entry, asked: Instant, cold: bool) {
        let (same_path, source, inode) = match self.nodes.get(&entry.id) {
            Some(node) => (
                node.bound.as_ref().is_some_and(|(path, _)| path == &entry.path),
                node.source(asked),
                node.inode.clone(),
            ),
            None => (false, None, self.add(entry.id, Some(entry.clone()), asked)),
        };
        inode.state().observe(entry, asked);
        // A listing binds new or relocated entries, but does not refresh an unchanged binding (C7).
        let bound = !(cold && same_path) && self.bind(entry.id, &entry.path, asked);
        if bound
            && entry.kind == Kind::Dir
            && let Some((source, at)) = source.filter(|(source, _)| source != &entry.path)
        {
            self.rebase(&source, &entry.path, at);
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
    pub(super) fn bind(&mut self, id: EntryId, path: &SpacePath, at: Instant) -> bool {
        let later = |node: Option<&Node>| {
            let bound = node.and_then(|node| node.bound.as_ref());
            bound.is_some_and(|(_, confirmed)| *confirmed >= at)
        };
        let holder = self.by_path.get(path).copied().filter(|holder| *holder != id);
        if later(self.nodes.get(&id)) || later(holder.and_then(|holder| self.nodes.get(&holder))) {
            return false;
        }
        if let Some(holder) = holder.and_then(|holder| self.nodes.get_mut(&holder)) {
            holder.unbind(at);
        }
        let Some(node) = self.nodes.get_mut(&id) else {
            return false;
        };
        if let Some((old, _)) = node.bound.replace((path.clone(), at))
            && old != *path
        {
            self.by_path.remove(&old);
        }
        node.last_path = None;
        self.by_path.insert(path.clone(), id);
        true
    }

    pub(super) fn confirm_parent(&mut self, id: EntryId, child: &SpacePath, at: Instant) {
        let Some(path) = child.parent() else {
            return;
        };
        let source = self.nodes.get(&id).and_then(|node| node.source(at));
        if self.bind(id, &path, at)
            && let Some((source, confirmed)) = source.filter(|(source, _)| source != &path)
        {
            self.rebase(&source, &path, confirmed);
        }
    }

    pub(super) fn rebase(&mut self, source: &SpacePath, destination: &SpacePath, at: Instant) {
        let moved: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| {
                let path = &node.bound.as_ref()?.0;
                Some((*id, path.rebase(source, destination)?))
            })
            .collect();
        for (id, path) in moved {
            self.bind(id, &path, at);
        }
    }

    /// Drops the least recently used entries that nothing holds, past `IDLE`.
    pub(super) fn evict(&mut self) {
        let nodes = &self.nodes;
        let used = |id: &EntryId| nodes.get(id).is_some_and(|node| node.inode.in_use());
        for id in self.idle.evict(IDLE, used) {
            if let Some((path, _)) = self.nodes.remove(&id).and_then(|node| node.bound) {
                self.by_path.remove(&path);
            }
        }
    }
}

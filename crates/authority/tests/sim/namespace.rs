//! Namespace mutations in the reference model.

use herdfs_core::{
    SpacePath,
    proto::{EntryId, Kind, Response},
};

use crate::model::{Model, Node};

pub(super) fn node(kind: Kind, mode: u32, target: Option<&String>, now_ms: i64) -> Node {
    Node {
        id: EntryId(0),
        kind,
        mode,
        mtime_ms: now_ms,
        target: target.cloned(),
        history: Vec::new(),
    }
}

impl Model {
    pub(super) fn node_mut(&mut self, space: &str, path: &SpacePath) -> &mut Node {
        self.spaces
            .get_mut(space)
            .and_then(|space| space.nodes.get_mut(path))
            .expect("node exists")
    }

    pub(super) fn add(&mut self, space: &str, path: &SpacePath, reply: &Response, node: Node) {
        let Response::Entry { entry } = reply else {
            panic!("expected an entry, got {reply:?}")
        };
        assert!(self.entry_ids.insert(entry.id), "entry id {:?} reused", entry.id);
        let nodes = &mut self.spaces.get_mut(space).expect("space exists").nodes;
        let created_ms = node.mtime_ms;
        nodes.insert(path.clone(), Node { id: entry.id, ..node });
        assert_eq!(*entry, self.entry(space, path));
        self.touch_parent(space, path, created_ms);
    }

    /// A directory's mtime moves when an entry is added to it or taken from it.
    pub(super) fn touch_parent(&mut self, space: &str, path: &SpacePath, now_ms: i64) {
        let Some(parent) = path.parent() else { return };
        self.node_mut(space, &parent).mtime_ms = now_ms;
    }

    pub(super) fn remove(&mut self, space: &str, path: &SpacePath) {
        let nodes = &mut self.spaces.get_mut(space).expect("space exists").nodes;
        let node = nodes.remove(path).expect("node exists");
        self.forget(&node.history);
        self.commits.retain(|_, (entry, ..)| *entry != node.id);
    }

    pub(super) fn rename(&mut self, space: &str, from: &SpacePath, to: &SpacePath, now_ms: i64) {
        if from == to {
            return;
        }
        if self.spaces[space].nodes.contains_key(to) {
            self.remove(space, to);
        }
        let nodes = &mut self.spaces.get_mut(space).expect("space exists").nodes;
        let moved: Vec<_> = nodes
            .keys()
            .filter(|path| path.is_within(from))
            .cloned()
            .collect();
        for old in moved {
            let node = nodes.remove(&old).expect("listed above");
            nodes.insert(old.rebase(from, to).expect("within from"), node);
        }
        self.touch_parent(space, from, now_ms);
        self.touch_parent(space, to, now_ms);
    }
}

//! Folding acknowledged replies into the model, checking each reply on the way.

use std::collections::BTreeMap;

use herdfs_authority::{Outcome, Principal};
use herdfs_core::{
    SpacePath,
    proto::{EntryId, Kind, Perm, Request, Response, VersionId},
};

use crate::model::{Model, Node, Space};

fn node(kind: Kind, mode: u32, target: Option<&String>, now_ms: i64) -> Node {
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
    /// Checks the reply to an acknowledged `request` and applies its effects.
    pub fn apply(&mut self, who: &Principal, request: &Request, outcome: &Outcome, now_ms: i64) {
        let reply = match outcome {
            Outcome::Sign { chunks } => return self.check_grant(who, request, chunks),
            Outcome::Reply(reply) => reply,
        };
        match request {
            Request::Whoami => {
                assert_eq!(
                    *reply,
                    Response::Me {
                        principal: who.user.clone(),
                        groups: who.groups.clone()
                    }
                );
            }
            Request::CreateSpace { space } => {
                let granted_seq = self.versions.keys().next_back().map_or(0, |id| id.0);
                let acl =
                    BTreeMap::from([((SpacePath::root(), who.user.clone()), (Perm::Admin, granted_seq))]);
                self.spaces.insert(
                    space.clone(),
                    Space {
                        nodes: BTreeMap::new(),
                        acl,
                    },
                );
                self.add(
                    space,
                    &SpacePath::root(),
                    reply,
                    node(Kind::Dir, 0o755, None, now_ms),
                );
            }
            Request::Fork { .. } => panic!("forks are adopted from a snapshot"),
            Request::Stat { space, path } => assert_eq!(
                *reply,
                Response::Entry {
                    entry: self.entry(space, path)
                }
            ),
            Request::Locate { space, entry } => {
                let path = self.node_by_id(space, *entry).expect("located").0;
                let expected = Response::Entry {
                    entry: self.entry(space, path),
                };
                assert_eq!(*reply, expected);
            }
            Request::Lookup { space, parent, name } => {
                let path = self.child_path(space, *parent, name).expect("looked up").0;
                let expected = Response::Entry {
                    entry: self.entry(space, &path),
                };
                assert_eq!(*reply, expected);
            }
            Request::List { space, entry } => {
                let path = self.node_by_id(space, *entry).expect("listed").0;
                self.check_list(space, path, reply);
            }
            Request::Open { space, entry } => {
                let path = self.node_by_id(space, *entry).expect("opened").0;
                let entry = self.entry(space, path);
                let version = entry.head.map(|id| self.versions[&id].clone());
                let writable = self.perm(space, path, who) >= Some(Perm::Write);
                assert_eq!(
                    *reply,
                    Response::Opened {
                        entry,
                        version,
                        writable
                    }
                );
            }
            Request::History { space, path } => {
                let (node, after) = (
                    &self.spaces[space].nodes[path],
                    self.readable_after(space, path, who),
                );
                let mut versions = self.history(node);
                versions.retain(|version| node.history.last() == Some(&version.id) || version.id.0 > after);
                assert_eq!(*reply, Response::History { versions });
                // contract: S8 — history before the caller's access stays hidden, except the head.
                let Response::History { versions } = reply else {
                    unreachable!()
                };
                let hidden = versions
                    .iter()
                    .find(|version| node.history.last() != Some(&version.id) && version.id.0 <= after);
                assert!(hidden.is_none(), "{} saw {hidden:?}", who.user);
            }
            Request::Purge { space, path } => {
                assert_eq!(*reply, Response::Done);
                let history = &mut self.node_mut(space, path).history;
                let purged = history
                    .drain(..history.len().saturating_sub(1))
                    .collect::<Vec<_>>();
                self.forget(&purged);
            }
            Request::DeleteSpace { space } => {
                assert_eq!(*reply, Response::Done);
                let deleted = self.spaces.remove(space).expect("space exists");
                let versions: Vec<_> = deleted
                    .nodes
                    .values()
                    .flat_map(|node| node.history.clone())
                    .collect();
                self.forget(&versions);
                let entries: Vec<_> = deleted.nodes.values().map(|node| node.id).collect();
                self.commits.retain(|_, (entry, ..)| !entries.contains(entry));
            }
            Request::Grant { .. } => panic!("Grant answered {reply:?}"),
            Request::Create {
                space,
                parent,
                name,
                mode,
            } => {
                let path = self.child_path(space, *parent, name).expect("created").0;
                self.add(space, &path, reply, node(Kind::File, *mode, None, now_ms));
            }
            Request::Mkdir {
                space,
                parent,
                name,
                mode,
            } => {
                let path = self.child_path(space, *parent, name).expect("successful mkdir").0;
                self.add(space, &path, reply, node(Kind::Dir, *mode, None, now_ms));
            }
            Request::Symlink {
                space,
                parent,
                name,
                target,
            } => {
                let path = self.child_path(space, *parent, name).expect("linked").0;
                self.add(
                    space,
                    &path,
                    reply,
                    node(Kind::Symlink, 0o777, Some(target), now_ms),
                );
            }
            Request::Remove { space, parent, name } => {
                // The answer is the entry as it was, so a client drops exactly its state.
                let path = self.child_path(space, *parent, name).expect("removed").0;
                let Response::Entry { entry } = reply else {
                    panic!("Remove answered {reply:?}")
                };
                assert_eq!(*entry, self.entry(space, &path));
                self.remove(space, &path);
                self.touch_parent(space, &path, now_ms);
            }
            Request::Rename {
                space,
                from_parent,
                from_name,
                to_parent,
                to_name,
            } => {
                let from = self
                    .child_path(space, *from_parent, from_name)
                    .expect("renamed")
                    .0;
                let to = self.child_path(space, *to_parent, to_name).expect("renamed").0;
                let entry = herdfs_core::proto::Entry {
                    path: to.clone(),
                    ..self.entry(space, &from)
                };
                assert_eq!(
                    *reply,
                    Response::Renamed {
                        from: from.clone(),
                        entry
                    }
                );
                self.rename(space, &from, &to, now_ms);
            }
            Request::SetAttr {
                space,
                entry,
                mode,
                mtime_ms,
            } => {
                let path = self.node_by_id(space, *entry).expect("changed").0.clone();
                let node = self.node_mut(space, &path);
                node.mode = mode.unwrap_or(node.mode);
                node.mtime_ms = mtime_ms.unwrap_or(node.mtime_ms);
                assert_eq!(
                    *reply,
                    Response::Entry {
                        entry: self.entry(space, &path)
                    }
                );
            }
            Request::Commit { .. } => self.commit(who, request, reply, now_ms),
            Request::Restore { space, version, .. } => self.restore(who, space, *version, reply, now_ms),
            Request::SetAcl {
                space,
                path,
                principal,
                perm,
            } => {
                assert_eq!(*reply, Response::Done);
                let granted_seq = self.versions.keys().next_back().map_or(0, |id| id.0);
                let acl = &mut self.spaces.get_mut(space).expect("space exists").acl;
                let key = (path.clone(), principal.clone());
                // A changed perm keeps the row's grant sequence (S8).
                match perm {
                    Some(perm) => acl.entry(key).or_insert((*perm, granted_seq)).0 = *perm,
                    None => drop(acl.remove(&key)),
                };
            }
        }
    }
    fn node_mut(&mut self, space: &str, path: &SpacePath) -> &mut Node {
        self.spaces
            .get_mut(space)
            .and_then(|space| space.nodes.get_mut(path))
            .expect("node exists")
    }
    fn add(&mut self, space: &str, path: &SpacePath, reply: &Response, node: Node) {
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
    fn touch_parent(&mut self, space: &str, path: &SpacePath, now_ms: i64) {
        let Some(parent) = path.parent() else { return };
        self.node_mut(space, &parent).mtime_ms = now_ms;
    }
    fn remove(&mut self, space: &str, path: &SpacePath) {
        let nodes = &mut self.spaces.get_mut(space).expect("space exists").nodes;
        let node = nodes.remove(path).expect("node exists");
        self.forget(&node.history);
        self.commits.retain(|_, (entry, ..)| *entry != node.id);
    }
    /// Versions are gone; the request ids that made them stay with their entry.
    pub fn forget(&mut self, gone: &[VersionId]) {
        self.acked.retain(|version, _| !gone.contains(version));
    }
    fn rename(&mut self, space: &str, from: &SpacePath, to: &SpacePath, now_ms: i64) {
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
    fn check_list(&self, space: &str, dir: &SpacePath, reply: &Response) {
        let Response::Entries { entries } = reply else {
            panic!("List answered {reply:?}")
        };
        let mut entries = entries.clone();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let children = self.spaces[space]
            .nodes
            .keys()
            .filter(|path| path.parent().as_ref() == Some(dir));
        let expected: Vec<_> = children.map(|path| self.entry(space, path)).collect();
        assert_eq!(entries, expected, "children of {dir}");
    }
}

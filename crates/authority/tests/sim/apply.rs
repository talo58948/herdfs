//! Folding acknowledged replies into the model, checking each reply on the way.

use std::collections::BTreeMap;

use herdfs_authority::{Outcome, Principal};
use herdfs_core::{
    ChunkId, SpacePath,
    chunk::total_len,
    proto::{EntryId, Kind, Perm, Request, Response, Version},
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
            Outcome::Sign { chunks, expires_ms } => {
                return self.check_grant(request, chunks, *expires_ms, now_ms);
            }
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
                let acl = BTreeMap::from([((SpacePath::root(), who.user.clone()), Perm::Admin)]);
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
            Request::List { space, path } => self.check_list(space, path, reply),
            Request::Open { space, path } => {
                let entry = self.entry(space, path);
                let version = entry.head.map(|id| self.versions[&id].clone());
                assert_eq!(*reply, Response::Opened { entry, version });
            }
            Request::History { space, path } => {
                let versions = self.history(&self.spaces[space].nodes[path]);
                assert_eq!(*reply, Response::History { versions });
            }
            Request::Grant { .. } => panic!("Grant answered {reply:?}"),
            Request::Create { space, path, mode } => {
                self.add(space, path, reply, node(Kind::File, *mode, None, now_ms));
            }
            Request::Mkdir { space, path, mode } => {
                self.add(space, path, reply, node(Kind::Dir, *mode, None, now_ms));
            }
            Request::Symlink { space, path, target } => {
                self.add(
                    space,
                    path,
                    reply,
                    node(Kind::Symlink, 0o777, Some(target), now_ms),
                );
            }
            Request::Remove { space, path } => {
                assert_eq!(*reply, Response::Done);
                self.remove(space, path);
            }
            Request::Rename { space, from, to } => {
                assert_eq!(*reply, Response::Done);
                self.rename(space, from, to);
            }
            Request::SetAttr {
                space,
                path,
                mode,
                mtime_ms,
            } => {
                let node = self.node_mut(space, path);
                node.mode = mode.unwrap_or(node.mode);
                node.mtime_ms = mtime_ms.unwrap_or(node.mtime_ms);
                assert_eq!(
                    *reply,
                    Response::Entry {
                        entry: self.entry(space, path)
                    }
                );
            }
            Request::Commit { .. } => self.commit(who, request, reply, now_ms),
            Request::Restore { space, path, version } => {
                let Response::Version { version: restored } = reply else {
                    panic!("Restore answered {reply:?}")
                };
                let old = &self.versions[version];
                let expected = Version {
                    author: who.user.clone(),
                    created_ms: now_ms,
                    ..old.clone()
                };
                assert_eq!(
                    *restored,
                    Version {
                        id: restored.id,
                        ..expected
                    }
                );
                self.add_version(space, restored.clone());
                assert_eq!(self.node_mut(space, path).history.last(), Some(&restored.id));
            }
            Request::SetAcl {
                space,
                path,
                principal,
                perm,
            } => {
                assert_eq!(*reply, Response::Done);
                let acl = &mut self.spaces.get_mut(space).expect("space exists").acl;
                let key = (path.clone(), principal.clone());
                match perm {
                    Some(perm) => acl.insert(key, *perm),
                    None => acl.remove(&key),
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
        nodes.insert(path.clone(), Node { id: entry.id, ..node });
        assert_eq!(*entry, self.entry(space, path));
    }

    fn remove(&mut self, space: &str, path: &SpacePath) {
        let nodes = &mut self.spaces.get_mut(space).expect("space exists").nodes;
        let node = nodes.remove(path).expect("node exists");
        // Versions go with their entry, and so do the request ids that made them.
        self.commits.retain(|_, version| !node.history.contains(version));
        self.acked.retain(|version, _| !node.history.contains(version));
    }

    fn rename(&mut self, space: &str, from: &SpacePath, to: &SpacePath) {
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
    }

    fn commit(&mut self, who: &Principal, request: &Request, reply: &Response, now_ms: i64) {
        let Request::Commit {
            space,
            entry,
            request_id,
            base,
            guarded,
            chunks,
        } = request
        else {
            unreachable!()
        };
        let Response::Version { version } = reply else {
            panic!("Commit answered {reply:?}")
        };
        let key = (who.user.clone(), request_id.clone());
        if let Some(first) = self.commits.get(&key) {
            // contract: C4a, C12 — a retried commit answers with its first version and adds none.
            assert_eq!(
                *version, self.versions[first],
                "retry of {request_id} made a new version"
            );
            return;
        }
        let size = total_len(chunks);
        let author = who.user.clone();
        let expected = Version {
            id: version.id,
            entry: *entry,
            size,
            author,
            created_ms: now_ms,
            chunks: chunks.clone(),
        };
        assert_eq!(*version, expected);
        self.add_version(space, version.clone());
        self.commits.insert(key, version.id);
        self.acked.insert(version.id, (space.clone(), *entry));
        if *guarded {
            *self.guarded_wins.entry((*entry, *base)).or_default() += 1;
        }
    }

    fn add_version(&mut self, space: &str, version: Version) {
        assert!(
            !self.versions.contains_key(&version.id),
            "version id {:?} reused",
            version.id
        );
        let nodes = self
            .spaces
            .get_mut(space)
            .expect("space exists")
            .nodes
            .values_mut();
        let node = nodes
            .into_iter()
            .find(|node| node.id == version.entry)
            .expect("entry exists");
        node.history.push(version.id);
        node.mtime_ms = version.created_ms;
        self.versions.insert(version.id, version);
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

    fn check_grant(&self, request: &Request, signed: &[ChunkId], expires_ms: i64, now_ms: i64) {
        let Request::Grant { version, chunks, .. } = request else {
            panic!("{request:?} signed")
        };
        let refs = &self.versions[version].chunks;
        let expected: Vec<_> = chunks.iter().map(|&index| refs[index as usize].id).collect();
        // contract: S2 — a grant covers only chunks of that version and expires within the TTL.
        assert_eq!(signed, expected);
        assert_eq!(expires_ms, now_ms + 5 * 60 * 1000);
    }
}

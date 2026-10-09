//! What the sim checks: the whole observed state after every step, and the
//! targeted invariants after commits, renames and revocations.

use std::collections::BTreeMap;

use herdfs_authority::{Outcome, Principal};
use herdfs_core::{
    SpacePath,
    proto::{Entry, EntryId, ErrorCode, Kind, Perm, Request, Response, Version, VersionId},
};

use crate::{
    AUDITOR, Sim,
    model::{Model, Node, Space},
};

/// One space as the auditor sees it: every entry with its history.
pub type Snapshot = BTreeMap<SpacePath, (Entry, Vec<Version>)>;

impl Sim {
    /// Checked after every step.
    pub fn check(&mut self) {
        let spaces: Vec<String> = self.model.spaces.keys().cloned().collect();
        for space in &spaces {
            let snapshot = self.observe(space);
            self.check_state(space, &snapshot);
            self.check_acked(space, &snapshot);
        }
        // contract: C6 — at most one guarded commit ever wins on a given base of an entry.
        let twice = self.model.guarded_wins.iter().find(|(_, wins)| **wins > 1);
        assert!(
            twice.is_none(),
            "two guarded commits won on (entry, base) {twice:?}"
        );
    }

    /// Every entry of `space` with its history, as the auditor sees them.
    pub fn observe(&mut self, space: &str) -> Snapshot {
        let root = self
            .stat(space, &SpacePath::root())
            .expect("the auditor reads every root");
        let root_id = root.id;
        let mut snapshot = Snapshot::from([(SpacePath::root(), (root, Vec::new()))]);
        let mut dirs = vec![root_id];
        while let Some(id) = dirs.pop() {
            let Response::Entries { entries } = self.audit(&Request::List {
                space: space.into(),
                entry: id,
            }) else {
                panic!("List answers entries");
            };
            for entry in entries {
                let history = match entry.kind {
                    Kind::File => self.history(space, &entry.path),
                    Kind::Dir => {
                        dirs.push(entry.id);
                        Vec::new()
                    }
                    Kind::Symlink => Vec::new(),
                };
                snapshot.insert(entry.path.clone(), (entry, history));
            }
        }
        snapshot
    }

    fn check_state(&self, space: &str, snapshot: &Snapshot) {
        let nodes = &self.model.spaces[space].nodes;
        let expected: Snapshot = nodes
            .iter()
            .map(|(path, node)| {
                (
                    path.clone(),
                    (self.model.entry(space, path), self.model.history(node)),
                )
            })
            .collect();
        // contract: C3, C5, C7, C13, C14 — namespace, heads and histories match the model, forks included.
        let differs = expected
            .keys()
            .chain(snapshot.keys())
            .find(|path| expected.get(*path) != snapshot.get(*path));
        if let Some(path) = differs {
            panic!(
                "{space}:{path}\n model: {:?}\n authority: {:?}",
                expected.get(path),
                snapshot.get(path)
            );
        }
    }

    fn check_acked(&self, space: &str, snapshot: &Snapshot) {
        // contract: C4b, C5 — every acknowledged commit stays in its entry's history, across restarts.
        for (version, (_, entry)) in self.model.acked.iter().filter(|(_, (at, _))| at == space) {
            let history = snapshot
                .values()
                .find(|(found, _)| found.id == *entry)
                .map(|(_, history)| history);
            let Some(history) = history else {
                panic!("{space}: entry {entry:?} of {version:?} is gone")
            };
            assert!(
                history.iter().any(|kept| kept.id == *version),
                "{space}: {version:?} left {entry:?}"
            );
        }
    }

    /// After an acknowledged unguarded commit.
    pub fn check_head(&mut self, space: &str, entry: EntryId, version: VersionId) {
        let path = self
            .model
            .node_by_id(space, entry)
            .expect("committed entry exists")
            .0
            .clone();
        // contract: C5 — the last acknowledged unguarded commit is the head.
        assert_eq!(self.stat(space, &path).map(|found| found.head), Ok(Some(version)));
    }

    /// After an acknowledged rename of `from`, whose subtree held `moved`.
    pub fn check_rename(
        &mut self,
        space: &str,
        from: &SpacePath,
        to: &SpacePath,
        moved: &[(SpacePath, EntryId)],
    ) {
        for (old, id) in moved {
            let new = old.rebase(from, to).expect("moved entries lie within from");
            // contract: C7 — the whole subtree moves at once and keeps its entries.
            assert_eq!(
                self.stat(space, &new).map(|found| found.id),
                Ok(*id),
                "{old} -> {new}"
            );
            assert_eq!(
                self.stat(space, old).map(|found| found.id),
                Err(ErrorCode::NotFound),
                "{old} stayed"
            );
        }
    }

    /// After a refused request by id: `Commit` names its entry only by id.
    pub fn check_refusal(&self, who: &Principal, message: &str) {
        for (space, known) in &self.model.spaces {
            let hidden = known
                .nodes
                .keys()
                .filter(|path| self.model.perm(space, path, who) < Some(Perm::Read));
            for path in hidden.filter(|path| !path.is_root()) {
                // contract: S3 — a refusal of a request by id names no path the caller cannot read.
                assert!(
                    !message.contains(path.as_str()),
                    "{} was told {message:?}",
                    who.user
                );
            }
        }
    }

    /// After an acknowledged removal of an ACL row on `subtree`.
    pub fn check_revocation(&mut self, space: &str, subtree: &SpacePath) {
        let files: Vec<_> = self.model.spaces[space]
            .nodes
            .iter()
            .filter(|(path, _)| path.is_within(subtree))
            .filter_map(|(path, node)| Some((path.clone(), node.id, *node.history.last()?)))
            .collect();
        for who in 0..AUDITOR {
            for (path, entry, version) in &files {
                let readable = self.model.perm(space, path, &self.actors[who]) >= Some(Perm::Read);
                let request = Request::Grant {
                    space: space.into(),
                    entry: *entry,
                    version: *version,
                    chunks: vec![],
                };
                let granted = self.exec(who, &request).is_ok();
                // contract: S4 — after a revocation, no grant on that subtree unless another row still grants read.
                assert!(
                    readable || !granted,
                    "{} kept a grant on {path}",
                    self.actors[who].user
                );
            }
        }
    }

    fn stat(&mut self, space: &str, path: &SpacePath) -> Result<Entry, ErrorCode> {
        let request = Request::Stat {
            space: space.into(),
            path: path.clone(),
        };
        self.now_ms += 1;
        match self.authority.handle(&self.actors[AUDITOR], request, self.now_ms) {
            Ok(Outcome::Reply(Response::Entry { entry })) => Ok(entry),
            Ok(other) => panic!("Stat answered {other:?}"),
            Err(error) => Err(error.code),
        }
    }

    fn history(&mut self, space: &str, path: &SpacePath) -> Vec<Version> {
        match self.audit(&Request::History {
            space: space.into(),
            path: path.clone(),
        }) {
            Response::History { versions } => versions,
            other => panic!("History answered {other:?}"),
        }
    }

    /// A read by the auditor, who may read everything.
    fn audit(&mut self, request: &Request) -> Response {
        self.now_ms += 1;
        match self
            .authority
            .handle(&self.actors[AUDITOR], request.clone(), self.now_ms)
        {
            Ok(Outcome::Reply(reply)) => reply,
            other => panic!("auditor's {request:?} answered {other:?}"),
        }
    }
}

impl Model {
    /// Builds the model of fork `name` of `source` from its snapshot.
    pub fn adopt_fork(&mut self, source: &str, name: &str, snapshot: &Snapshot) {
        let parent = self.spaces[source].clone();
        // contract: C13 — a fork holds the parent's whole namespace and every acknowledged head.
        assert!(
            snapshot.keys().eq(parent.nodes.keys()),
            "fork {name} differs from {source}: {snapshot:?}"
        );
        let mut fork = Space {
            nodes: BTreeMap::new(),
            acl: BTreeMap::new(),
            access: Vec::new(),
        };
        for (path, node) in parent.nodes {
            let (entry, copies) = &snapshot[&path];
            assert!(
                self.entry_ids.insert(entry.id),
                "fork {name} reused entry id {:?}",
                entry.id
            );
            let history = match (node.history.last(), copies.as_slice()) {
                (None, []) => Vec::new(),
                (Some(head), [copy]) => {
                    assert_eq!(
                        *copy,
                        Version {
                            id: copy.id,
                            entry: entry.id,
                            ..self.versions[head].clone()
                        }
                    );
                    assert!(
                        self.versions.insert(copy.id, copy.clone()).is_none(),
                        "fork reused {:?}",
                        copy.id
                    );
                    vec![copy.id]
                }
                (head, copies) => panic!("{path}: fork copied {copies:?} for head {head:?}"),
            };
            fork.nodes.insert(
                path,
                Node {
                    id: entry.id,
                    history,
                    ..node
                },
            );
        }
        self.spaces.insert(name.to_owned(), fork);
        for ((path, principal), (perm, _)) in parent.acl {
            self.change_acl(name, &path, &principal, Some(perm));
        }
        for (path, (entry, _)) in snapshot {
            assert_eq!(*entry, self.entry(name, path));
        }
    }
}

//! Folding acknowledged commits and restores into the model, and checking grants.

use herdfs_authority::Principal;
use herdfs_core::{
    ChunkId,
    chunk::total_len,
    proto::{Request, Response, Version, VersionId},
};

use crate::model::Model;

impl Model {
    pub fn commit(&mut self, who: &Principal, request: &Request, reply: &Response, now_ms: i64) {
        let Request::Commit {
            space,
            entry,
            request_id,
            base,
            guarded,
            chunks,
            mtime_ms,
        } = request
        else {
            unreachable!("only a Commit is folded in as one")
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
        let (size, author) = (total_len(chunks), who.user.clone());
        let expected = Version {
            id: version.id,
            entry: *entry,
            size,
            author,
            created_ms: now_ms,
            chunks: chunks.clone(),
        };
        assert_eq!(*version, expected);
        self.add_version(space, version.clone(), mtime_ms.unwrap_or(now_ms));
        self.commits.insert(key, version.id);
        self.acked.insert(version.id, (space.clone(), *entry));
        if *guarded {
            *self.guarded_wins.entry((*entry, *base)).or_default() += 1;
        }
    }

    pub fn restore(&mut self, who: &Principal, space: &str, old: VersionId, reply: &Response, now_ms: i64) {
        let Response::Version { version: restored } = reply else {
            panic!("Restore answered {reply:?}")
        };
        let old = &self.versions[&old];
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
        self.add_version(space, restored.clone(), now_ms);
    }

    fn add_version(&mut self, space: &str, version: Version, mtime_ms: i64) {
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
        node.mtime_ms = mtime_ms;
        self.versions.insert(version.id, version);
    }

    pub fn check_grant(&self, request: &Request, signed: &[ChunkId]) {
        let Request::Grant { version, chunks, .. } = request else {
            panic!("{request:?} signed")
        };
        let refs = &self.versions[version].chunks;
        let expected: Vec<_> = chunks.iter().map(|&index| refs[index as usize].id).collect();
        // contract: S2 — a grant covers only chunks of that version.
        assert_eq!(signed, expected);
    }
}

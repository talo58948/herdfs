//! Commits, restores and grants in the model: the outcome of a commit, and
//! folding acknowledged commits and restores in.

use herdfs_authority::Principal;
use herdfs_core::{
    ChunkId, ChunkRef,
    chunk::total_len,
    proto::{EntryId, ErrorCode, Kind, Perm, Request, Response, Version, VersionId},
};

use crate::model::{Model, Outcome, ensure};

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
        if let Some((_, first, _)) = self.commits.get(&key) {
            // Once its version left retention or was purged, the head answers.
            let node = self
                .node_by_id(space, *entry)
                .expect("a request id's entry exists")
                .1;
            let answer = if node.history.contains(first) {
                first
            } else {
                node.history.last().expect("a head")
            };
            // contract: C4a, C5, C12 — a retried commit answers without committing again.
            assert_eq!(
                *version, self.versions[answer],
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
        self.commits.insert(key, (*entry, version.id, now_ms));
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

    pub fn check_grant(&self, who: &Principal, request: &Request, signed: &[ChunkId]) {
        let Request::Grant {
            space,
            entry,
            version,
            chunks,
        } = request
        else {
            panic!("{request:?} signed")
        };
        let (path, node) = self.node_by_id(space, *entry).expect("a granted entry exists");
        // contract: S8 — a grant never covers a version made before the caller could read the entry.
        assert!(
            self.visible(space, path, node, *version, who),
            "{} saw {version:?} of {path}",
            who.user
        );
        let refs = &self.versions[version].chunks;
        let expected: Vec<_> = chunks.iter().map(|&index| refs[index as usize].id).collect();
        // contract: S2 — a grant covers only chunks of that version.
        assert_eq!(signed, expected);
    }

    #[allow(clippy::too_many_arguments, reason = "mirrors the fields of Request::Commit")]
    pub fn commit_outcome(
        &self,
        who: &Principal,
        space: &str,
        entry: EntryId,
        request_id: &str,
        base: Option<VersionId>,
        guarded: bool,
        chunks: &[ChunkRef],
    ) -> Outcome {
        self.space(space)?;
        let retry = (who.user.clone(), request_id.to_owned());
        if let Some((first, version, _)) = self.commits.get(&retry) {
            ensure(*first == entry, ErrorCode::Invalid)?;
            let (path, node) = self.node_by_id(space, entry).ok_or(ErrorCode::NotFound)?;
            self.require(who, space, path, Perm::Read)?;
            // A replay after its version went answers with the head, to a reader only.
            return match node.history.contains(version) {
                true => ensure(
                    self.visible(space, path, node, *version, who),
                    ErrorCode::NotFound,
                ),
                false => Ok(()),
            };
        }
        let (path, node) = self.node_by_id(space, entry).ok_or(ErrorCode::NotFound)?;
        self.require(who, space, path, Perm::Write)?;
        ensure(node.kind == Kind::File, ErrorCode::Invalid)?;
        let base_chunks = match base {
            Some(base) if node.history.contains(&base) && self.visible(space, path, node, base, who) => {
                self.versions[&base].chunks.as_slice()
            }
            // Gone, of another entry, or hidden by S8 alike: the writer commits again without a base.
            Some(_) => return Err(ErrorCode::Conflict),
            None => &[],
        };
        let known = |chunk: &ChunkRef| {
            let uploaded = self
                .uploads
                .get(&(who.user.clone(), chunk.id))
                .map(|(len, _)| *len);
            base_chunks.contains(chunk) || uploaded == Some(chunk.len)
        };
        ensure(chunks.iter().all(known), ErrorCode::Invalid)?;
        ensure(
            !guarded || node.history.last().copied() == base,
            ErrorCode::Conflict,
        )
    }
}

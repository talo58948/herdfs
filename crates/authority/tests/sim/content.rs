//! Steps on file content: commits, guarded races, retries, restores and grants.

use herdfs_authority::Outcome;
use herdfs_core::{
    ChunkRef,
    proto::{EntryId, ErrorCode, Kind, Perm, Request, Response, VersionId},
};

use crate::{NO_ENTRY, Sim};

impl Sim {
    pub fn commit(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let (entry, history) = self.commit_target(&space);
        let base = match self.rng.below(10) {
            0..=4 => history.last().copied(),
            5 | 6 => self.rng.pick(&history),
            7 | 8 => None,
            _ => self.rng.pick(&self.versions()),
        };
        let base_chunks = base
            .map(|base| self.model.versions[&base].chunks.clone())
            .unwrap_or_default();
        let chunks = (0..self.rng.below(4))
            .map(|_| self.commit_chunk(who, &space, &base_chunks))
            .collect();
        let guarded = self.rng.chance(30);
        // Sometimes the writer set the file's mtime, as `cp -p` does.
        let mtime_ms = self.rng.chance(20).then(|| self.rng.below(1 << 30) as i64);
        let request_id = format!("r{}", self.sent.len());
        let request = Request::Commit {
            space: space.clone(),
            entry,
            request_id,
            base,
            guarded,
            chunks,
            mtime_ms,
        };
        self.sent.push((who, request.clone()));
        let acked = self.exec(who, &request);
        if let (Ok(_), Request::Commit { chunks, .. }) = (&acked, &request) {
            let true_len = |chunk: &ChunkRef| {
                self.chunks
                    .iter()
                    .find(|made| made.id == chunk.id)
                    .map(|made| made.len)
            };
            // contract: S6 — no acknowledged commit claims a length its chunk does not have.
            assert!(
                chunks.iter().all(|chunk| true_len(chunk) == Some(chunk.len)),
                "{request:?} was acknowledged"
            );
        }
        if let Ok(outcome) = acked
            && !guarded
        {
            self.check_head(&space, entry, version_of(&outcome));
        }
        if self.rng.chance(25) {
            // The reply was lost, so the client sends the commit again, maybe to a restarted authority.
            if self.rng.chance(50) {
                self.restart();
            }
            self.send(who, &request);
        }
    }

    /// Mostly a file of `space`; sometimes another entry, one of another space, or none.
    fn commit_target(&mut self, space: &str) -> (EntryId, Vec<VersionId>) {
        let spaces: Vec<_> = self.model.spaces.keys().cloned().collect();
        let from = match self.rng.chance(90) {
            true => space.to_owned(),
            false => self.rng.pick(&spaces).expect("space s always exists"),
        };
        let files_only = self.rng.chance(85);
        let nodes: Vec<_> = self.model.spaces.get(&from).map_or_else(Vec::new, |known| {
            let nodes = known
                .nodes
                .values()
                .filter(|node| !files_only || node.kind == Kind::File);
            nodes.map(|node| (node.id, node.history.clone())).collect()
        });
        match self.rng.chance(95) {
            true => self.rng.pick(&nodes).unwrap_or((NO_ENTRY, Vec::new())),
            false => (NO_ENTRY, Vec::new()),
        }
    }

    fn commit_chunk(&mut self, who: usize, space: &str, base_chunks: &[ChunkRef]) -> ChunkRef {
        let chunk = match self.rng.below(20) {
            0..=11 => self.upload(who, space),
            12..=18 => self.rng.pick(base_chunks).unwrap_or_else(|| self.chunk()),
            _ => self
                .rng
                .pick(&self.chunks.clone())
                .unwrap_or_else(|| self.chunk()),
        };
        // Sometimes the client claims a length the chunk does not have.
        let len = if self.rng.chance(5) {
            chunk.len + 1
        } else {
            chunk.len
        };
        ChunkRef { len, ..chunk }
    }

    /// Several principals commit guarded on the same head of one file.
    pub fn race(&mut self) {
        let space = self.space();
        let files: Vec<_> = self.model.spaces.get(&space).map_or_else(Vec::new, |known| {
            let files = known.nodes.values().filter(|node| node.kind == Kind::File);
            files
                .map(|node| (node.id, node.history.last().copied()))
                .collect()
        });
        let Some((entry, base)) = self.rng.pick(&files) else {
            return;
        };
        let (mut wins, mut conflicts) = (0, 0);
        for _ in 0..2 + self.rng.below(3) {
            let who = self.actor();
            let chunks = vec![self.upload(who, &space)];
            let request_id = format!("r{}", self.sent.len());
            let request = Request::Commit {
                space: space.clone(),
                entry,
                request_id,
                base,
                guarded: true,
                chunks,
                mtime_ms: None,
            };
            self.sent.push((who, request.clone()));
            match self.exec(who, &request) {
                Ok(_) => wins += 1,
                Err(ErrorCode::Conflict) => conflicts += 1,
                Err(_) => {}
            }
        }
        // contract: C6 — of the guarded commits on one base, exactly one succeeds and the rest conflict.
        assert!(
            wins + conflicts == 0 || wins == 1,
            "{wins} winners and {conflicts} conflicts"
        );
    }

    /// Resends an earlier commit, mostly an acknowledged one, as a client does
    /// after losing the reply; sometimes to another entry under the same id.
    pub fn retry(&mut self) {
        let any = self.rng.chance(20);
        let acked = |(who, request): &&(usize, Request)| match request {
            Request::Commit { request_id, .. } => {
                let key = (self.actors[*who].user.clone(), request_id.clone());
                any || self.model.commits.contains_key(&key)
            }
            _ => false,
        };
        let candidates: Vec<_> = self.sent.iter().filter(acked).cloned().collect();
        let Some((who, mut request)) = self.rng.pick(&candidates) else {
            return;
        };
        let Request::Commit {
            space,
            entry,
            request_id,
            ..
        } = &mut request
        else {
            unreachable!()
        };
        let first = self
            .model
            .commits
            .get(&(self.actors[who].user.clone(), request_id.clone()))
            .copied();
        // Sometimes a buggy client reuses the id for a commit to another file.
        let others: Vec<_> = self
            .model
            .spaces
            .get(space.as_str())
            .map_or_else(Vec::new, |known| {
                known
                    .nodes
                    .values()
                    .map(|node| node.id)
                    .filter(|id| id != entry)
                    .collect()
            });
        if let Some(other) = self.rng.pick(&others)
            && self.rng.chance(30)
        {
            *entry = other;
        }
        // The id is reused on another entry than the one its first commit made a version of.
        let reused = first.is_some_and(|first| self.model.versions[&first].entry != *entry);
        let got = self.exec(who, &request);
        if reused {
            // contract: C4a, C12 — an id reused on another entry commits nothing and returns no version.
            assert_eq!(got.err(), Some(ErrorCode::Invalid), "{request:?}");
        }
    }

    pub fn restore(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let path = self.existing_path(&space);
        let history = self
            .model
            .node(&space, &path)
            .map(|node| node.history.clone())
            .unwrap_or_default();
        let version = match self.rng.chance(70) {
            true => self.rng.pick(&history),
            false => self.rng.pick(&self.versions()),
        };
        let Some(version) = version else { return };
        self.send(who, &Request::Restore { space, path, version });
    }

    pub fn grant(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let path = self.existing_path(&space);
        let node = self.model.node(&space, &path).ok();
        let (entry, history) = node.map_or((NO_ENTRY, Vec::new()), |node| (node.id, node.history.clone()));
        let version = match self.rng.chance(80) {
            true => self.rng.pick(&history),
            false => self.rng.pick(&self.versions()),
        };
        let Some(version) = version else { return };
        let count = self.model.versions[&version].chunks.len();
        let chunks = match self.rng.chance(2) {
            true => vec![0; 4097],
            false => (0..self.rng.below(3))
                .map(|_| self.rng.below(count + 1) as u32)
                .collect(),
        };
        let readable = self.model.perm(&space, &path, &self.actors[who]) >= Some(Perm::Read);
        let granted = self
            .exec(
                who,
                &Request::Grant {
                    space,
                    entry,
                    version,
                    chunks,
                },
            )
            .is_ok();
        // contract: S2 — no grant succeeds without read permission at that moment.
        assert!(
            readable || !granted,
            "{} got a grant without read",
            self.actors[who].user
        );
    }
    /// Records a fresh chunk as uploaded by `who`, if the model allows it.
    pub fn upload(&mut self, who: usize, space: &str) -> ChunkRef {
        let chunk = self.chunk();
        let principal = &self.actors[who];
        let want = self.model.upload_outcome(principal, space);
        self.now_ms += 1;
        let got = self.authority.record_upload(principal, space, chunk, self.now_ms);
        assert_eq!(
            got.map_err(|error| error.code),
            want,
            "upload by {} to {space}",
            principal.user
        );
        if want.is_ok() {
            self.model
                .uploads
                .insert((principal.user.clone(), chunk.id), chunk.len);
        }
        chunk
    }
}

fn version_of(outcome: &Outcome) -> VersionId {
    match outcome {
        Outcome::Reply(Response::Version { version }) => version.id,
        other => panic!("a commit answered {other:?}"),
    }
}

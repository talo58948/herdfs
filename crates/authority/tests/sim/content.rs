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
        let request_id = format!("r{}", self.sent.len());
        let request = Request::Commit {
            space: space.clone(),
            entry,
            request_id,
            base,
            guarded,
            chunks,
        };
        self.sent.push((who, request.clone()));
        if let Ok(outcome) = self.exec(who, &request)
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
        match self.rng.below(20) {
            0..=11 => self.upload(who, space),
            12..=18 => self.rng.pick(base_chunks).unwrap_or_else(|| self.chunk()),
            _ => self
                .rng
                .pick(&self.chunks.clone())
                .unwrap_or_else(|| self.chunk()),
        }
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

    /// Resends an earlier commit, mostly an acknowledged one, as a client does after losing the reply.
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
        if let Some((who, request)) = self.rng.pick(&candidates) {
            self.send(who, &request);
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
        let history = self
            .model
            .node(&space, &path)
            .map(|node| node.history.clone())
            .unwrap_or_default();
        let version = match self.rng.chance(80) {
            true => self.rng.pick(&history),
            false => self.rng.pick(&self.versions()),
        };
        let Some(version) = version else { return };
        let count = self.model.versions[&version].chunks.len();
        let chunks = (0..self.rng.below(3))
            .map(|_| self.rng.below(count + 1) as u32)
            .collect();
        let readable = self.model.perm(&space, &path, &self.actors[who]) >= Some(Perm::Read);
        let granted = self
            .exec(
                who,
                &Request::Grant {
                    space,
                    path,
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
        let got = self
            .authority
            .record_upload(principal, space, chunk.id, self.now_ms);
        assert_eq!(
            got.map_err(|error| error.code),
            want,
            "upload by {} to {space}",
            principal.user
        );
        if want.is_ok() {
            self.model.uploads.insert((principal.user.clone(), chunk.id));
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

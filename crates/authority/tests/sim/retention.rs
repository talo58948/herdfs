//! Collection in the sim: random retention policies at sim time, checked
//! against the model's own pruning.

use std::collections::BTreeSet;

use herdfs_authority::Policy;
use herdfs_core::ChunkId;

use crate::{Sim, model::Model};

/// Request ids stay at least this long whatever the retention (docs/architecture.md, Limits).
const REQUEST_MEMORY_MS: i64 = 30 * 24 * 60 * 60 * 1000;

impl Sim {
    pub fn collect(&mut self) {
        let policy = Policy {
            versions: self.rng.below(4) as u32,
            max_age_ms: self.rng.below(20_000) as i64,
            grace_ms: self.rng.below(10_000) as i64,
        };
        self.now_ms += 1;
        let collected = self.authority.collect(self.now_ms, &policy).unwrap();
        assert_eq!(
            collected.pruned,
            self.model.collect(self.now_ms, &policy),
            "versions pruned by {policy:?}"
        );
        // contract: C15 — live are exactly the chunks of retained versions and of fresh uploads.
        assert_eq!(collected.live, self.model.live(), "live chunks after {policy:?}");
        // contract: C14, S7 — no retained version loses a chunk.
        let retained = self.model.spaces.values().flat_map(|space| space.nodes.values());
        let versions = retained
            .flat_map(|node| &node.history)
            .map(|id| &self.model.versions[id]);
        let lost = versions
            .flat_map(|version| &version.chunks)
            .find(|chunk| !collected.live.contains(&chunk.id));
        assert!(lost.is_none(), "a retained version lost {lost:?}");
    }
}

impl Model {
    /// Prunes as the authority must, returning how many versions went.
    fn collect(&mut self, now_ms: i64, policy: &Policy) -> usize {
        let (cutoff, keep) = (now_ms - policy.max_age_ms, policy.versions as usize);
        for version in self.versions.values_mut() {
            version.created_ms = version.created_ms.min(now_ms);
        }
        let versions = &self.versions;
        let mut pruned = Vec::new();
        for node in self
            .spaces
            .values_mut()
            .flat_map(|space| space.nodes.values_mut())
        {
            let (len, head) = (node.history.len(), node.history.last().copied());
            let kept = |(index, id): &(usize, _)| {
                len - index <= keep || versions[id].created_ms > cutoff || Some(*id) == head
            };
            let (kept, gone): (Vec<_>, Vec<_>) = node.history.iter().copied().enumerate().partition(kept);
            node.history = kept.into_iter().map(|(_, id)| id).collect();
            pruned.extend(gone.into_iter().map(|(_, id)| id));
        }
        for (_, at_ms) in self.uploads.values_mut() {
            *at_ms = (*at_ms).min(now_ms);
        }
        self.uploads
            .retain(|_, (_, at_ms)| *at_ms > now_ms - policy.grace_ms);
        self.forget(&pruned);
        let forgotten = now_ms
            .saturating_sub(policy.max_age_ms.max(REQUEST_MEMORY_MS))
            .saturating_sub(policy.grace_ms);
        self.commits.retain(|_, (.., at_ms)| *at_ms >= forgotten);
        pruned.len()
    }

    /// Chunks of every retained version, in every space, and of every upload still recorded.
    fn live(&self) -> BTreeSet<ChunkId> {
        let nodes = self.spaces.values().flat_map(|space| space.nodes.values());
        let versions = nodes.flat_map(|node| &node.history).map(|id| &self.versions[id]);
        let mut live: BTreeSet<_> = versions
            .flat_map(|version| &version.chunks)
            .map(|chunk| chunk.id)
            .collect();
        live.extend(self.uploads.keys().map(|(_, id)| *id));
        live
    }
}

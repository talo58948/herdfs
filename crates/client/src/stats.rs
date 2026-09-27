//! Counters behind the `user.herdfs.stats` xattr on the mount root.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use serde_json::{Map, Value, json};

use crate::strategy::Strategy;

#[derive(Default)]
pub struct Stats {
    pub bytes_fetched: AtomicU64,
    pub chunks_fetched: AtomicU64,
    pub cache_hits: AtomicU64,
    /// `Grant` calls; each covers every chunk one read or prefetch batch lacked.
    pub grants: AtomicU64,
    pub commits: AtomicU64,
    /// Jev's confident answers, its unsure ones, and asks that failed or timed out (P2).
    pub jev_answers: AtomicU64,
    pub jev_unsure: AtomicU64,
    pub jev_errors: AtomicU64,
    decisions: [AtomicU64; Strategy::ALL.len()],
}

pub fn bump(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Relaxed);
}

impl Stats {
    pub fn decided(&self, strategy: Strategy) {
        bump(&self.decisions[strategy as usize], 1);
    }

    /// The counters as JSON. Pending commits are counted by the journal.
    pub fn json(&self, pending_commits: usize) -> Vec<u8> {
        let decisions: Map<String, Value> = Strategy::ALL
            .into_iter()
            .map(|strategy| {
                (
                    strategy.name().into(),
                    self.decisions[strategy as usize].load(Relaxed).into(),
                )
            })
            .collect();
        json!({
            "bytes_fetched": self.bytes_fetched.load(Relaxed),
            "chunks_fetched": self.chunks_fetched.load(Relaxed),
            "cache_hits": self.cache_hits.load(Relaxed),
            "grants": self.grants.load(Relaxed),
            "commits": self.commits.load(Relaxed),
            "pending_commits": pending_commits,
            "jev_answers": self.jev_answers.load(Relaxed),
            "jev_unsure": self.jev_unsure.load(Relaxed),
            "jev_errors": self.jev_errors.load(Relaxed),
            "decisions": decisions,
        })
        .to_string()
        .into_bytes()
    }
}

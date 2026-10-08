//! Counters behind the `user.herdfs.stats` xattr on the mount root.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use serde_json::json;

use crate::strategy::{AccessPlan, Keep, Prefetch, ReadAhead};

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
    prefetch: [AtomicU64; Prefetch::ALL.len()],
    read_ahead: [AtomicU64; ReadAhead::ALL.len()],
    keep: [AtomicU64; Keep::ALL.len()],
}

pub fn bump(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Relaxed);
}

impl Stats {
    pub fn decided(&self, plan: AccessPlan) {
        bump(&self.prefetch[plan.prefetch as usize], 1);
        bump(&self.read_ahead[plan.read_ahead as usize], 1);
        bump(&self.keep[plan.keep as usize], 1);
    }

    /// The counters as JSON. Pending commits are counted by the journal, the round trip (seconds)
    /// by the remote.
    pub fn json(&self, pending_commits: usize, round_trip: Option<f64>) -> Vec<u8> {
        let prefetch = |setting: Prefetch| (setting.name(), self.prefetch[setting as usize].load(Relaxed));
        let read_ahead =
            |setting: ReadAhead| (setting.name(), self.read_ahead[setting as usize].load(Relaxed));
        let keep = |setting: Keep| (setting.name(), self.keep[setting as usize].load(Relaxed));
        json!({
            "bytes_fetched": self.bytes_fetched.load(Relaxed),
            "chunks_fetched": self.chunks_fetched.load(Relaxed),
            "cache_hits": self.cache_hits.load(Relaxed),
            "grants": self.grants.load(Relaxed),
            "commits": self.commits.load(Relaxed),
            "pending_commits": pending_commits,
            "round_trip_ms": round_trip.map(|seconds| seconds * 1e3),
            "jev_answers": self.jev_answers.load(Relaxed),
            "jev_unsure": self.jev_unsure.load(Relaxed),
            "jev_errors": self.jev_errors.load(Relaxed),
            "decisions": {
                "prefetch": BTreeMap::from(Prefetch::ALL.map(prefetch)),
                "read_ahead": BTreeMap::from(ReadAhead::ALL.map(read_ahead)),
                "keep": BTreeMap::from(Keep::ALL.map(keep)),
            },
        })
        .to_string()
        .into_bytes()
    }
}

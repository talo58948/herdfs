//! Collection: pruning versions outside retention, and the chunks still live.

use std::collections::BTreeSet;

use herdfs_core::{ChunkId, ChunkRef};
use rusqlite::{Connection, params};

use crate::Authority;

/// What collection keeps besides each entry's head.
#[derive(Clone, Debug)]
pub struct Policy {
    /// The newest versions of each entry, kept whatever their age.
    pub versions: u32,
    /// Versions younger than this are kept.
    pub max_age_ms: i64,
    /// A commit's uploads stay live until its newest is older than this, so the commit may
    /// still use them.
    pub grace_ms: i64,
}

/// What one collection pruned, and what it left live.
#[derive(Debug, PartialEq, Eq)]
pub struct Collected {
    pub pruned: usize,
    /// Chunk lists no remaining version used.
    pub dropped: usize,
    /// Every chunk of a retained version anywhere, or of a recent upload.
    pub live: BTreeSet<ChunkId>,
}

/// Versions past the newest `?1` of their entry, older than `?2`, and not its head.
const PRUNE: &str = "DELETE FROM versions WHERE id IN (
    SELECT id FROM (
        SELECT v.id, v.created_ms, e.head,
               row_number() OVER (PARTITION BY v.entry ORDER BY v.id DESC) AS newest
        FROM versions v JOIN entries e ON e.id = v.entry)
    WHERE newest > ?1 AND created_ms < ?2 AND id IS NOT head)";

/// Uploads for commits whose newest upload is older than `?1`. A commit's uploads go together,
/// so a file that takes longer than the grace to upload keeps its first chunks (C4a).
const STALE_UPLOADS: &str = "DELETE FROM uploads WHERE (principal, request_id) IN (
    SELECT principal, request_id FROM uploads GROUP BY principal, request_id HAVING max(at_ms) < ?1)";

impl Authority {
    /// Prunes versions outside retention, drops chunk lists, uploads and request
    /// ids that nothing needs, and returns the chunks still live, across every space.
    pub fn collect(&mut self, now_ms: i64, policy: &Policy) -> anyhow::Result<Collected> {
        let tx = self.db.transaction()?;
        let pruned = tx.execute(PRUNE, params![policy.versions, now_ms - policy.max_age_ms])?;
        let dropped = tx.execute(
            "DELETE FROM manifests WHERE hash NOT IN (SELECT manifest FROM versions)",
            [],
        )?;
        tx.execute(STALE_UPLOADS, [now_ms - policy.grace_ms])?;
        let forgotten = now_ms
            .saturating_sub(policy.max_age_ms)
            .saturating_sub(policy.grace_ms);
        tx.execute("DELETE FROM commits WHERE at_ms < ?1", [forgotten])?;
        let mut live = listed(&tx)?;
        live.extend(uploaded(&tx, i64::MIN)?);
        tx.commit()?;
        Ok(Collected {
            pruned,
            dropped,
            live,
        })
    }

    /// Chunks uploaded at or after `since_ms`.
    pub fn uploaded_since(&mut self, since_ms: i64) -> anyhow::Result<BTreeSet<ChunkId>> {
        uploaded(&self.db, since_ms)
    }
}

/// Every chunk some chunk list names.
fn listed(db: &Connection) -> anyhow::Result<BTreeSet<ChunkId>> {
    let mut query = db.prepare_cached("SELECT chunks FROM manifests")?;
    let mut live = BTreeSet::new();
    for list in query.query_map([], |row| row.get::<_, String>(0))? {
        let chunks: Vec<ChunkRef> = serde_json::from_str(&list?)?;
        live.extend(chunks.into_iter().map(|chunk| chunk.id));
    }
    Ok(live)
}

fn uploaded(db: &Connection, since_ms: i64) -> anyhow::Result<BTreeSet<ChunkId>> {
    let mut query = db.prepare_cached("SELECT chunk FROM uploads WHERE at_ms >= ?1")?;
    let rows = query.query_map([since_ms], |row| row.get::<_, String>(0))?;
    rows.map(|chunk| Ok(chunk?.parse()?)).collect()
}

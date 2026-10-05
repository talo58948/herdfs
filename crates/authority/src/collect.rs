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
    /// Every chunk of a retained version anywhere.
    pub(crate) retained: BTreeSet<ChunkId>,
    /// Every chunk of a retained version anywhere, or of a recent upload.
    pub live: BTreeSet<ChunkId>,
    pub(crate) upload_mark: UploadMark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UploadMark(i64);

/// Versions past the newest `?1` of their entry, created at or before `?2`, and not its head.
const PRUNE: &str = "DELETE FROM versions WHERE id IN (
    SELECT id FROM (
        SELECT v.id, v.created_ms, e.head,
               row_number() OVER (PARTITION BY v.entry ORDER BY v.id DESC) AS newest
        FROM versions v JOIN entries e ON e.id = v.entry)
    WHERE newest > ?1 AND created_ms <= ?2 AND id IS NOT head)";

/// Uploads for commits whose newest upload is at or before `?1`. A commit's uploads go together,
/// so a file that takes longer than the grace to upload keeps its first chunks (C4a).
const STALE_UPLOADS: &str = "DELETE FROM uploads WHERE (principal, request_id) IN (
    SELECT principal, request_id FROM uploads GROUP BY principal, request_id HAVING max(at_ms) <= ?1)";

/// Request ids are kept at least this long, whatever the retention, so a client that lost a
/// commit's answer and returns within it confirms that commit instead of committing it again.
const REQUEST_MEMORY_MS: i64 = 30 * 24 * 60 * 60 * 1000;

impl Authority {
    /// Prunes versions outside retention, drops chunk lists, uploads and request
    /// ids that nothing needs, and returns the chunks still live, across every space.
    pub fn collect(&mut self, now_ms: i64, policy: &Policy) -> anyhow::Result<Collected> {
        let tx = self.db.transaction()?;
        // A corrected fast clock left these in the future: they age from this collection.
        tx.execute("UPDATE uploads SET at_ms = ?1 WHERE at_ms > ?1", [now_ms])?;
        tx.execute(
            "UPDATE versions SET created_ms = ?1 WHERE created_ms > ?1",
            [now_ms],
        )?;
        let pruned = tx.execute(PRUNE, params![policy.versions, now_ms - policy.max_age_ms])?;
        let dropped = tx.execute(
            "DELETE FROM manifests WHERE hash NOT IN (SELECT manifest FROM versions)",
            [],
        )?;
        tx.execute(STALE_UPLOADS, [now_ms - policy.grace_ms])?;
        let upload_mark = tx.query_row("SELECT coalesce(max(seq), 0) FROM uploads", [], |row| row.get(0))?;
        let forgotten = now_ms
            .saturating_sub(policy.max_age_ms.max(REQUEST_MEMORY_MS))
            .saturating_sub(policy.grace_ms);
        tx.execute("DELETE FROM commits WHERE at_ms < ?1", [forgotten])?;
        let retained = listed(&tx)?;
        let mut live = retained.clone();
        live.extend(uploaded(&tx)?);
        tx.commit()?;
        Ok(Collected {
            pruned,
            dropped,
            retained,
            live,
            upload_mark: UploadMark(upload_mark),
        })
    }

    /// Records a completed collection and returns chunks whose unreferenced grace has elapsed.
    pub(crate) fn expired(
        &mut self,
        unretained: &BTreeSet<ChunkId>,
        started: i64,
        grace_ms: i64,
        ttl_ms: i64,
    ) -> anyhow::Result<BTreeSet<ChunkId>> {
        let tx = self.db.transaction()?;
        let previous = tx.query_row(
            "SELECT coalesce(started_ms, created_ms) FROM collection WHERE id = 1",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let floor = started - grace_ms + ttl_ms;
        let since = if previous > started {
            floor
        } else {
            previous.max(floor)
        };
        let chunks = serde_json::to_string(unretained)?;
        tx.execute("UPDATE collection SET started_ms = ?1 WHERE id = 1", [started])?;
        tx.execute(
            "DELETE FROM unreferenced
             WHERE since_ms > ?2 OR chunk NOT IN (SELECT value FROM json_each(?1))",
            params![chunks, started],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO unreferenced (chunk, since_ms)
             SELECT value, ?2 FROM json_each(?1)",
            params![chunks, since],
        )?;
        let mut query = tx.prepare("SELECT chunk FROM unreferenced WHERE since_ms <= ?1")?;
        let rows = query.query_map([started - grace_ms], |row| row.get::<_, String>(0))?;
        let expired = rows
            .map(|chunk| Ok(chunk?.parse()?))
            .collect::<anyhow::Result<_>>()?;
        drop(query);
        tx.commit()?;
        Ok(expired)
    }

    /// Chunks recorded after `mark`.
    pub(crate) fn uploaded_after(&mut self, mark: UploadMark) -> anyhow::Result<BTreeSet<ChunkId>> {
        let mut query = self
            .db
            .prepare_cached("SELECT chunk FROM uploads WHERE seq > ?1")?;
        let rows = query.query_map([mark.0], |row| row.get::<_, String>(0))?;
        rows.map(|chunk| Ok(chunk?.parse()?)).collect()
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

fn uploaded(db: &Connection) -> anyhow::Result<BTreeSet<ChunkId>> {
    let mut query = db.prepare_cached("SELECT chunk FROM uploads")?;
    let rows = query.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|chunk| Ok(chunk?.parse()?)).collect()
}

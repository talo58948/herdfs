//! The schema and the entry and version rows every request reads.

use herdfs_core::{
    ChunkRef, SpacePath,
    chunk::total_len,
    proto::{Entry, EntryId, ErrorCode, Kind, Version, VersionId},
};
use rusqlite::{OptionalExtension, Row, params, types::Type};

use crate::{Call, Result, refuse};

// Ids are AUTOINCREMENT so a removed entry's or version's id is never reused.
pub(crate) const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    space TEXT NOT NULL,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    mode INTEGER NOT NULL,
    mtime_ms INTEGER NOT NULL,
    head INTEGER,
    target TEXT,
    UNIQUE (space, path)
);
CREATE TABLE IF NOT EXISTS versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entry INTEGER NOT NULL REFERENCES entries (id) ON DELETE CASCADE,
    size INTEGER NOT NULL,
    author TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    chunks TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS versions_by_entry ON versions (entry);
CREATE TABLE IF NOT EXISTS acl (
    space TEXT NOT NULL,
    path TEXT NOT NULL,
    principal TEXT NOT NULL,
    perm TEXT NOT NULL,
    PRIMARY KEY (space, principal, path)
);
CREATE TABLE IF NOT EXISTS uploads (
    principal TEXT NOT NULL,
    chunk TEXT NOT NULL,
    len INTEGER NOT NULL,
    PRIMARY KEY (principal, chunk)
);
CREATE TABLE IF NOT EXISTS commits (
    principal TEXT NOT NULL,
    request_id TEXT NOT NULL,
    version INTEGER NOT NULL REFERENCES versions (id) ON DELETE CASCADE,
    PRIMARY KEY (principal, request_id)
);
CREATE INDEX IF NOT EXISTS commits_by_version ON commits (version);
";

const ENTRY: &str = "SELECT e.id, e.path, e.kind, e.mode, e.mtime_ms, e.head, coalesce(v.size, 0), e.target
    FROM entries e LEFT JOIN versions v ON v.id = e.head WHERE e.space = ?1";
const VERSION: &str = "SELECT id, entry, size, author, created_ms, chunks FROM versions";

pub(crate) fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "file",
        Kind::Dir => "dir",
        Kind::Symlink => "symlink",
    }
}

/// Byte-order bounds `(lo, hi)` of the paths strictly below `dir`. The
/// root has no upper bound; for any other dir, `'0'` is the byte after `'/'`.
pub(crate) fn below(dir: &SpacePath) -> (String, Option<String>) {
    if dir.is_root() {
        return (String::new(), None);
    }
    (format!("{}/", dir.as_str()), Some(format!("{}0", dir.as_str())))
}

impl Call<'_> {
    pub(crate) fn find(&self, space: &str, path: &SpacePath) -> Result<Option<Entry>> {
        let mut query = self.db.prepare_cached(&format!("{ENTRY} AND e.path = ?2"))?;
        Ok(query
            .query_row(params![space, path.as_str()], entry_row)
            .optional()?)
    }

    pub(crate) fn entry(&self, space: &str, path: &SpacePath) -> Result<Entry> {
        self.find(space, path)?
            .ok_or_else(|| refuse(ErrorCode::NotFound, format!("no entry at {path}")))
    }

    pub(crate) fn entry_by_id(&self, space: &str, id: EntryId) -> Result<Entry> {
        let mut query = self.db.prepare_cached(&format!("{ENTRY} AND e.id = ?2"))?;
        query
            .query_row(params![space, id.0], entry_row)
            .optional()?
            .ok_or_else(|| refuse(ErrorCode::NotFound, format!("no entry {}", id.0)))
    }

    pub(crate) fn children(&self, space: &str, dir: &SpacePath) -> Result<Vec<Entry>> {
        let (lo, hi) = below(dir);
        let sql = format!(
            "{ENTRY} AND e.path > ?2 AND (?3 IS NULL OR e.path < ?3)
             AND instr(substr(e.path, length(?2) + 1), '/') = 0 ORDER BY e.path"
        );
        let mut query = self.db.prepare_cached(&sql)?;
        let rows = query.query_map(params![space, lo, hi], entry_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn has_children(&self, space: &str, dir: &SpacePath) -> Result<bool> {
        let (lo, hi) = below(dir);
        let mut query = self.db.prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM entries WHERE space = ?1 AND path > ?2 AND (?3 IS NULL OR path < ?3))",
        )?;
        Ok(query.query_row(params![space, lo, hi], |row| row.get(0))?)
    }

    pub(crate) fn version(&self, id: VersionId) -> Result<Option<Version>> {
        let mut query = self.db.prepare_cached(&format!("{VERSION} WHERE id = ?1"))?;
        Ok(query.query_row([id.0], version_row).optional()?)
    }

    /// Versions of `entry`, oldest first.
    pub(crate) fn versions(&self, entry: EntryId) -> Result<Vec<Version>> {
        let mut query = self
            .db
            .prepare_cached(&format!("{VERSION} WHERE entry = ?1 ORDER BY id"))?;
        let rows = query.query_map([entry.0], version_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds a version by the caller and makes it the entry's head, modified at `mtime_ms`.
    pub(crate) fn add_version(&self, entry: &Entry, chunks: &[ChunkRef], mtime_ms: i64) -> Result<Version> {
        let size = total_len(chunks);
        let encoded = serde_json::to_string(chunks)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let id = self
            .db
            .prepare_cached(
                "INSERT INTO versions (entry, size, author, created_ms, chunks)
                 VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
            )?
            .query_row(
                params![entry.id.0, size, self.who.user, self.now_ms, encoded],
                |row| row.get(0),
            )?;
        self.db
            .prepare_cached("UPDATE entries SET head = ?2, mtime_ms = ?3 WHERE id = ?1")?
            .execute(params![entry.id.0, id, mtime_ms])?;
        Ok(Version {
            id: VersionId(id),
            entry: entry.id,
            size,
            author: self.who.user.clone(),
            created_ms: self.now_ms,
            chunks: chunks.to_vec(),
        })
    }
}

fn entry_row(row: &Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: EntryId(row.get(0)?),
        path: parse(row, 1, |text| SpacePath::try_from(text).ok())?,
        kind: parse(row, 2, |text| {
            [Kind::File, Kind::Dir, Kind::Symlink]
                .into_iter()
                .find(|kind| kind_name(*kind) == text)
        })?,
        mode: row.get(3)?,
        mtime_ms: row.get(4)?,
        head: row.get::<_, Option<u64>>(5)?.map(VersionId),
        size: row.get(6)?,
        target: row.get(7)?,
    })
}

fn version_row(row: &Row) -> rusqlite::Result<Version> {
    Ok(Version {
        id: VersionId(row.get(0)?),
        entry: EntryId(row.get(1)?),
        size: row.get(2)?,
        author: row.get(3)?,
        created_ms: row.get(4)?,
        chunks: parse(row, 5, |text| serde_json::from_str(text).ok())?,
    })
}

/// Reads text column `index` through `parse`, which rejects bad values.
pub(crate) fn parse<T>(
    row: &Row,
    index: usize,
    parse: impl FnOnce(&str) -> Option<T>,
) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    parse(&text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, format!("bad value {text:?}").into())
    })
}

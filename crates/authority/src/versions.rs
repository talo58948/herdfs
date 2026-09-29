//! File content requests: open, grant, commit, history, restore and uploads.

use std::collections::HashSet;

use herdfs_core::{
    ChunkId, ChunkRef, SpacePath,
    proto::{Entry, EntryId, ErrorCode, Kind, Perm, Response, Version, VersionId},
};
use rusqlite::{OptionalExtension, params};

use crate::{Call, Outcome, Result, refuse};

/// Chunks one grant may cover, so no request makes the server sign without bound.
const MAX_GRANT: usize = 4096;

impl Call<'_> {
    pub(crate) fn open(&self, space: &str, path: &SpacePath) -> Result<Response> {
        self.require(space, path, Perm::Read)?;
        let writable = self.may(space, path, Perm::Write)?;
        let entry = self.entry(space, path)?;
        let version = match entry.head {
            Some(head) => self.version(head)?,
            None => None,
        };
        Ok(Response::Opened {
            entry,
            version,
            writable,
        })
    }

    /// By entry id, so a file renamed while open stays readable.
    pub(crate) fn grant(
        &self,
        space: &str,
        entry: EntryId,
        version: VersionId,
        indices: &[u32],
    ) -> Result<Outcome> {
        if indices.len() > MAX_GRANT {
            return Err(refuse(
                ErrorCode::Invalid,
                format!("at most {MAX_GRANT} chunks per grant"),
            ));
        }
        let entry = self.entry_by_id(space, entry)?;
        self.require(space, &entry.path, Perm::Read)?;
        let version = self.visible_version(space, &entry, version)?.ok_or_else(|| {
            refuse(
                ErrorCode::NotFound,
                format!("no version {} of entry {}", version.0, entry.id.0),
            )
        })?;
        let chunks = indices
            .iter()
            .map(|&index| {
                let chunk = usize::try_from(index)
                    .ok()
                    .and_then(|index| version.chunks.get(index));
                chunk
                    .map(|chunk| chunk.id)
                    .ok_or_else(|| refuse(ErrorCode::Invalid, format!("no chunk {index}")))
            })
            .collect::<Result<_>>()?;
        Ok(Outcome::Sign { chunks })
    }

    /// Entry ids are not secret: an unknown id is `NotFound` before any permission
    /// check. Its path may be, so no refusal here names it. Request ids are
    /// kept for the longer of retain-days and 30 days, plus the grace; a client
    /// offline longer than that could commit the same write twice (Limits).
    #[expect(clippy::too_many_arguments, reason = "the fields of Request::Commit")]
    pub(crate) fn commit(
        &self,
        space: &str,
        entry: EntryId,
        request_id: &str,
        base: Option<VersionId>,
        guarded: bool,
        chunks: &[ChunkRef],
        mtime_ms: Option<i64>,
    ) -> Result<Response> {
        self.require_space(space)?;
        if let Some((first, version)) = self.committed(request_id)? {
            if first != entry {
                return Err(refuse(ErrorCode::Invalid, "request id reused for another entry"));
            }
            let entry = self.entry_by_id(space, entry)?;
            if let Some(version) = self.version_of(&entry, version)? {
                return Ok(Response::Version { version });
            }
            return match self.may(space, &entry.path, Perm::Read)? {
                true => Ok(Response::Version {
                    version: self.head(&entry)?,
                }),
                false => Ok(Response::Done),
            };
        }
        let entry = self.entry_by_id(space, entry)?;
        self.require(space, &entry.path, Perm::Write)?;
        if entry.kind != Kind::File {
            return Err(refuse(
                ErrorCode::Invalid,
                format!("entry {} is not a file", entry.id.0),
            ));
        }
        let base_chunks = match base {
            Some(base) => self.base_chunks(space, &entry, base)?,
            None => HashSet::new(),
        };
        let distinct: HashSet<_> = chunks.iter().map(|chunk| (chunk.id, chunk.len)).collect();
        for (id, len) in distinct.difference(&base_chunks) {
            if self.uploaded_len(*id)? != Some(*len) {
                let message = format!("chunk {id} of {len} bytes is neither in the base nor uploaded by you");
                return Err(refuse(ErrorCode::Invalid, message));
            }
        }
        if guarded && entry.head != base {
            return Err(refuse(
                ErrorCode::Conflict,
                format!("entry {} has moved past its base", entry.id.0),
            ));
        }
        let version = self.add_version(&entry, chunks, mtime_ms.unwrap_or(self.now_ms))?;
        self.db
            .prepare_cached(
                "DELETE FROM unreferenced WHERE chunk IN (
                    SELECT json_extract(chunk.value, '$.id')
                    FROM versions version
                    JOIN manifests manifest ON manifest.hash = version.manifest,
                         json_each(manifest.chunks) chunk
                    WHERE version.id = ?1
                    EXCEPT
                    SELECT json_extract(chunk.value, '$.id')
                    FROM versions version
                    JOIN manifests manifest ON manifest.hash = version.manifest,
                         json_each(manifest.chunks) chunk
                    WHERE version.id = ?2)",
            )?
            .execute(params![version.id.0, base.map(|base| base.0)])?;
        self.db
            .prepare_cached(
                "INSERT INTO commits (principal, request_id, entry, version, at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![self.who.user, request_id, entry.id.0, version.id.0, self.now_ms])?;
        Ok(Response::Version { version })
    }

    pub(crate) fn history(&self, space: &str, path: &SpacePath) -> Result<Response> {
        self.require(space, path, Perm::Read)?;
        let entry = self.entry(space, path)?;
        let after = self.readable_after(space, path)?;
        let mut versions = self.versions(entry.id)?;
        versions.retain(|version| entry.head == Some(version.id) || version.id.0 > after as u64);
        Ok(Response::History { versions })
    }

    /// Deletes every version of the entry but its head; the next collection
    /// erases the bytes only they referenced.
    pub(crate) fn purge(&self, space: &str, path: &SpacePath) -> Result<Response> {
        self.require(space, path, Perm::Write)?;
        let entry = self.entry(space, path)?;
        self.db
            .prepare_cached("DELETE FROM versions WHERE entry = ?1 AND id IS NOT ?2")?
            .execute(params![entry.id.0, entry.head.map(|head| head.0)])?;
        Ok(Response::Done)
    }

    pub(crate) fn restore(&self, space: &str, path: &SpacePath, version: VersionId) -> Result<Response> {
        self.require(space, path, Perm::Write)?;
        let entry = self.entry(space, path)?;
        let old = self.visible_version(space, &entry, version)?.ok_or_else(|| {
            refuse(
                ErrorCode::NotFound,
                format!("no version {} of this entry", version.0),
            )
        })?;
        Ok(Response::Version {
            version: self.add_version(&entry, &old.chunks, self.now_ms)?,
        })
    }

    /// Uploads need write somewhere in the space: they are only useful to commit.
    pub(crate) fn record_upload(&self, space: &str, request_id: &str, chunk: ChunkRef) -> Result<()> {
        self.require_somewhere(space, Perm::Write)?;
        let id = chunk.id.to_string();
        // Re-uploading garbage can make it grantable before another collection sees it live.
        self.db
            .prepare_cached("DELETE FROM unreferenced WHERE chunk = ?1")?
            .execute([&id])?;
        self.db
            .prepare_cached(
                "INSERT OR REPLACE INTO uploads (principal, chunk, len, request_id, at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![self.who.user, id, chunk.len, request_id, self.now_ms])?;
        Ok(())
    }

    /// `id` as a version of `entry` the caller may see, or `None` just as if
    /// it did not exist: under S8 a version older than the caller's access
    /// to the entry's path is hidden, except the head.
    fn visible_version(&self, space: &str, entry: &Entry, id: VersionId) -> Result<Option<Version>> {
        let Some(version) = self.version_of(entry, id)? else {
            return Ok(None);
        };
        let after = self.readable_after(space, &entry.path)?;
        Ok((entry.head == Some(version.id) || version.id.0 > after as u64).then_some(version))
    }

    /// The chunks of `base`. A base that is gone, of another entry, or hidden
    /// from the caller gets the same `Conflict`, so the answer reveals nothing;
    /// the caller commits again without a base, uploading every chunk.
    fn base_chunks(&self, space: &str, entry: &Entry, base: VersionId) -> Result<HashSet<(ChunkId, u32)>> {
        let Some(version) = self.visible_version(space, entry, base)? else {
            return Err(refuse(ErrorCode::Conflict, "base is not available"));
        };
        Ok(version.chunks.iter().map(|chunk| (chunk.id, chunk.len)).collect())
    }

    fn version_of(&self, entry: &Entry, id: VersionId) -> Result<Option<Version>> {
        Ok(self.version(id)?.filter(|version| version.entry == entry.id))
    }

    /// The entry and version an earlier commit with this request id made.
    fn committed(&self, request_id: &str) -> Result<Option<(EntryId, VersionId)>> {
        let mut query = self
            .db
            .prepare_cached("SELECT entry, version FROM commits WHERE principal = ?1 AND request_id = ?2")?;
        let row = |row: &rusqlite::Row| Ok((EntryId(row.get(0)?), VersionId(row.get(1)?)));
        Ok(query
            .query_row(params![self.who.user, request_id], row)
            .optional()?)
    }

    /// The head version of an entry that has been committed to.
    fn head(&self, entry: &Entry) -> Result<Version> {
        let head = match entry.head {
            Some(head) => self.version(head)?,
            None => None,
        };
        head.ok_or_else(|| {
            refuse(
                ErrorCode::NotFound,
                format!("entry {} has no version", entry.id.0),
            )
        })
    }

    /// The length the caller uploaded chunk `id` with, if they did.
    fn uploaded_len(&self, id: ChunkId) -> Result<Option<u32>> {
        let mut query = self
            .db
            .prepare_cached("SELECT len FROM uploads WHERE principal = ?1 AND chunk = ?2")?;
        Ok(query
            .query_row(params![self.who.user, id.to_string()], |row| row.get(0))
            .optional()?)
    }
}

//! File content requests: open, grant, commit, history, restore and uploads.

use herdfs_core::{
    ChunkId, ChunkRef, SpacePath,
    proto::{Entry, EntryId, ErrorCode, Kind, Perm, Response, Version, VersionId},
};
use rusqlite::{OptionalExtension, params};

use crate::{Call, GRANT_TTL_MS, Outcome, Result, refuse};

impl Call<'_> {
    pub(crate) fn open(&self, space: &str, path: &SpacePath) -> Result<Response> {
        self.require(space, path, Perm::Read)?;
        let entry = self.entry(space, path)?;
        let version = match entry.head {
            Some(head) => self.version(head)?,
            None => None,
        };
        Ok(Response::Opened { entry, version })
    }

    pub(crate) fn grant(
        &self,
        space: &str,
        path: &SpacePath,
        version: VersionId,
        indices: &[u32],
    ) -> Result<Outcome> {
        self.require(space, path, Perm::Read)?;
        let entry = self.entry(space, path)?;
        let version = self
            .version_of(&entry, version)?
            .ok_or_else(|| refuse(ErrorCode::NotFound, format!("no version {} of {path}", version.0)))?;
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
        Ok(Outcome::Sign {
            chunks,
            expires_ms: self.now_ms + GRANT_TTL_MS,
        })
    }

    /// Entry ids are not secret: an unknown id is `NotFound` before any permission
    /// check. Its path may be, so no refusal here names it.
    pub(crate) fn commit(
        &self,
        space: &str,
        entry: EntryId,
        request_id: &str,
        base: Option<VersionId>,
        guarded: bool,
        chunks: &[ChunkRef],
    ) -> Result<Response> {
        self.require_space(space)?;
        if let Some(version) = self.committed(request_id)? {
            if version.entry != entry {
                return Err(refuse(ErrorCode::Invalid, "request id reused for another entry"));
            }
            return Ok(Response::Version { version });
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
            Some(base) => {
                let version = self.version_of(&entry, base)?;
                version
                    .ok_or_else(|| refuse(ErrorCode::Invalid, "base is not a version of this entry"))?
                    .chunks
            }
            None => Vec::new(),
        };
        for chunk in chunks {
            if !base_chunks.contains(chunk) && self.uploaded_len(chunk.id)? != Some(chunk.len) {
                let message = format!(
                    "chunk {} of {} bytes is neither in the base nor uploaded by you",
                    chunk.id, chunk.len
                );
                return Err(refuse(ErrorCode::Invalid, message));
            }
        }
        if guarded && entry.head != base {
            return Err(refuse(
                ErrorCode::Conflict,
                format!("entry {} has moved past its base", entry.id.0),
            ));
        }
        let version = self.add_version(&entry, chunks)?;
        self.db
            .prepare_cached("INSERT INTO commits (principal, request_id, version) VALUES (?1, ?2, ?3)")?
            .execute(params![self.who.user, request_id, version.id.0])?;
        Ok(Response::Version { version })
    }

    pub(crate) fn history(&self, space: &str, path: &SpacePath) -> Result<Response> {
        self.require(space, path, Perm::Read)?;
        let entry = self.entry(space, path)?;
        Ok(Response::History {
            versions: self.versions(entry.id)?,
        })
    }

    pub(crate) fn restore(&self, space: &str, path: &SpacePath, version: VersionId) -> Result<Response> {
        self.require(space, path, Perm::Write)?;
        let entry = self.entry(space, path)?;
        let old = self
            .version_of(&entry, version)?
            .ok_or_else(|| refuse(ErrorCode::NotFound, format!("no version {} of {path}", version.0)))?;
        Ok(Response::Version {
            version: self.add_version(&entry, &old.chunks)?,
        })
    }

    /// Uploads need write somewhere in the space: they are only useful to commit.
    pub(crate) fn record_upload(&self, space: &str, chunk: ChunkRef) -> Result<()> {
        self.require_somewhere(space, Perm::Write)?;
        self.db
            .prepare_cached("INSERT OR REPLACE INTO uploads (principal, chunk, len) VALUES (?1, ?2, ?3)")?
            .execute(params![self.who.user, chunk.id.to_string(), chunk.len])?;
        Ok(())
    }

    fn version_of(&self, entry: &Entry, id: VersionId) -> Result<Option<Version>> {
        Ok(self.version(id)?.filter(|version| version.entry == entry.id))
    }

    /// The version an earlier commit with this request id made.
    fn committed(&self, request_id: &str) -> Result<Option<Version>> {
        let mut query = self
            .db
            .prepare_cached("SELECT version FROM commits WHERE principal = ?1 AND request_id = ?2")?;
        let id = query
            .query_row(params![self.who.user, request_id], |row| row.get(0))
            .optional()?;
        match id {
            Some(id) => self.version(VersionId(id)),
            None => Ok(None),
        }
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

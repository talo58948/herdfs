//! The authority protocol. Each call is one JSON [`Request`] sent to
//! `POST /v1/rpc` with a bearer token, answered by a [`Response`] or, with a
//! non-2xx status, an [`Error`]. Chunk bytes are uploaded for the commit that
//! will name them with `PUT /v1/spaces/{space}/commits/{request_id}/chunks/{id}`
//! and downloaded from presigned URLs, so they never travel inside RPC bodies.

use serde::{Deserialize, Serialize};

use crate::{ChunkRef, SpacePath};

mod access;
pub use access::{AclRow, SpaceAccess};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VersionId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// Access levels. Each level includes the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Perm {
    Read,
    Write,
    Admin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub path: SpacePath,
    pub kind: Kind,
    pub mode: u32,
    pub mtime_ms: i64,
    /// Current content. `None` for directories, symlinks and files that
    /// were created but never committed.
    pub head: Option<VersionId>,
    pub size: u64,
    pub target: Option<String>,
}

/// An immutable file version. It belongs to exactly one entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub id: VersionId,
    pub entry: EntryId,
    pub size: u64,
    pub author: String,
    pub created_ms: i64,
    pub chunks: Vec<ChunkRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Whoami,
    /// Spaces where the caller has a user or group grant.
    Spaces,
    /// Creates a space with the caller as its admin.
    CreateSpace {
        space: String,
    },
    /// Copies `space`'s namespace, ACLs and head versions into a new space
    /// `name`. No chunk bytes are copied. Needs admin on `space`.
    Fork {
        space: String,
        name: String,
    },
    Stat {
        space: String,
        path: SpacePath,
    },
    /// The entry with this id, at its current path, so a client can follow
    /// renames made elsewhere. Needs read on that path.
    Locate {
        space: String,
        entry: EntryId,
    },
    /// The child named within `parent`. Needs read on its path; a parent that
    /// is not a directory answers `NotFound`.
    Lookup {
        space: String,
        parent: EntryId,
        name: String,
    },
    List {
        space: String,
        entry: EntryId,
    },
    /// The entry and its head version, including the chunk list.
    Open {
        space: String,
        entry: EntryId,
    },
    /// Presigned download URLs for chunks of `version`, by chunk index.
    /// `version` must belong to `entry`, and the caller needs read on the
    /// entry's current path, so a file renamed while open stays readable.
    Grant {
        space: String,
        entry: EntryId,
        version: VersionId,
        chunks: Vec<u32>,
    },
    /// Creates an empty file entry with no version yet.
    Create {
        space: String,
        parent: EntryId,
        /// One name within `parent`.
        name: String,
        mode: u32,
    },
    Mkdir {
        space: String,
        parent: EntryId,
        /// One name within `parent`.
        name: String,
        mode: u32,
    },
    Symlink {
        space: String,
        parent: EntryId,
        /// One name within `parent`.
        name: String,
        target: String,
    },
    /// Removes a file, a symlink or an empty directory. Answers with the
    /// removed entry, so a client knows exactly which entry went.
    Remove {
        space: String,
        parent: EntryId,
        /// One name within `parent`.
        name: String,
    },
    /// Moves an entry, or a directory with its subtree, in one step.
    /// Replaces an existing file, or an empty directory, at the destination.
    Rename {
        space: String,
        from_parent: EntryId,
        /// One name within `from_parent`.
        from_name: String,
        to_parent: EntryId,
        /// One name within `to_parent`.
        to_name: String,
    },
    SetAttr {
        space: String,
        entry: EntryId,
        mode: Option<u32>,
        mtime_ms: Option<i64>,
    },
    /// Makes `chunks` the entry's new head version. Every chunk must be in
    /// `base` or uploaded by the caller. A `guarded` commit fails with
    /// `Conflict` unless the head is still `base`; any commit fails with
    /// `Conflict` if `base` is no longer retained, and the caller retries
    /// without a base, uploading every chunk. Retrying with the same
    /// `request_id` returns the first result instead of committing again.
    /// A retried `request_id` from the same caller confirms the first commit:
    /// its own version if still retained (the author may always see it), else
    /// the current head if readable, else `Done`, which also means committed.
    /// `mtime_ms` keeps a modification time the writer set (`cp -p`);
    /// without it the commit time is used.
    Commit {
        space: String,
        entry: EntryId,
        request_id: String,
        base: Option<VersionId>,
        guarded: bool,
        chunks: Vec<ChunkRef>,
        mtime_ms: Option<i64>,
    },
    /// All versions of the entry at `path`, oldest first.
    History {
        space: String,
        path: SpacePath,
    },
    /// Makes an old version of the entry the new head, as a new version.
    Restore {
        space: String,
        path: SpacePath,
        version: VersionId,
    },
    /// Deletes every version of the entry at `path` except its head, so
    /// their bytes are erased by the next collection. Needs write.
    Purge {
        space: String,
        path: SpacePath,
    },
    /// Deletes a space with all its entries, versions and grants, so its
    /// bytes are erased by the next collection. Needs admin on its root.
    DeleteSpace {
        space: String,
    },
    /// Sets `principal`'s access on `path` and below; `None` removes it.
    /// Principals are `user:<sub>` or `group:<name>`.
    SetAcl {
        space: String,
        path: SpacePath,
        principal: String,
        perm: Option<Perm>,
    },
    /// Explicit grants on `path` and its ancestors. Needs admin on `path`.
    Access {
        space: String,
        path: SpacePath,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Me {
        principal: String,
        groups: Vec<String>,
    },
    Spaces {
        spaces: Vec<SpaceAccess>,
    },
    Access {
        grants: Vec<AclRow>,
    },
    Entry {
        entry: Entry,
    },
    Entries {
        entries: Vec<Entry>,
    },
    /// `writable` says whether the caller may commit to the entry, so a
    /// write-open can fail at once rather than at commit.
    Opened {
        entry: Entry,
        version: Option<Version>,
        writable: bool,
    },
    /// URLs valid for `ttl_ms` from when the reply was sent. Relative, so a
    /// client's clock skew does not matter.
    Grants {
        urls: Vec<String>,
        ttl_ms: u64,
    },
    Version {
        version: Version,
    },
    History {
        versions: Vec<Version>,
    },
    /// The source path and moved entry, resolved in the rename transaction.
    Renamed {
        from: SpacePath,
        entry: Entry,
    },
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    Exists,
    NotEmpty,
    NotDir,
    IsDir,
    Denied,
    Unauthenticated,
    Conflict,
    Invalid,
    Unavailable,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests;

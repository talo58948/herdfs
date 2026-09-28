//! Spaces: creating, forking, and checking that one exists.

use herdfs_core::{
    SpacePath,
    proto::{ErrorCode, Perm, Response},
};
use rusqlite::params;

use crate::{Call, Result, refuse};

const ROOT_MODE: u32 = 0o755;

/// Copies the entries, head versions and ACL rows of space `?1` into the new space `?2`.
const FORK: [&str; 4] = [
    "INSERT INTO entries (space, path, kind, mode, mtime_ms, target)
     SELECT ?2, path, kind, mode, mtime_ms, target FROM entries WHERE space = ?1",
    "INSERT INTO versions (entry, size, author, created_ms, manifest)
     SELECT copy.id, head.size, head.author, head.created_ms, head.manifest
     FROM entries source JOIN versions head ON head.id = source.head
     JOIN entries copy ON copy.space = ?2 AND copy.path = source.path
     WHERE source.space = ?1",
    "UPDATE entries SET head = (SELECT id FROM versions WHERE entry = entries.id)
     WHERE space = ?2",
    "INSERT INTO acl (space, path, principal, perm) SELECT ?2, path, principal, perm FROM acl WHERE space = ?1",
];

/// `[a-z0-9][a-z0-9._-]{0,62}`
fn is_valid_name(name: &str) -> bool {
    let alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let mut bytes = name.bytes();
    name.len() <= 63
        && bytes.next().is_some_and(alphanumeric)
        && bytes.all(|byte| alphanumeric(byte) || b"._-".contains(&byte))
}

impl Call<'_> {
    pub(crate) fn create_space(&self, name: &str) -> Result<Response> {
        self.require_new_space(name)?;
        self.db
            .prepare_cached(
                "INSERT INTO entries (space, path, kind, mode, mtime_ms) VALUES (?1, '', 'dir', ?2, ?3)",
            )?
            .execute(params![name, ROOT_MODE, self.now_ms])?;
        self.make_admin(name)?;
        Ok(Response::Entry {
            entry: self.entry(name, &SpacePath::root())?,
        })
    }

    /// Copies metadata only, so it takes time in proportion to the entry count.
    pub(crate) fn fork(&self, space: &str, name: &str) -> Result<Response> {
        self.require(space, &SpacePath::root(), Perm::Admin)?;
        self.require_new_space(name)?;
        for sql in FORK {
            self.db.prepare_cached(sql)?.execute(params![space, name])?;
        }
        Ok(Response::Entry {
            entry: self.entry(name, &SpacePath::root())?,
        })
    }

    pub(crate) fn require_space(&self, space: &str) -> Result<()> {
        if self.find(space, &SpacePath::root())?.is_none() {
            return Err(refuse(ErrorCode::NotFound, format!("no space {space}")));
        }
        Ok(())
    }

    fn require_new_space(&self, name: &str) -> Result<()> {
        if !is_valid_name(name) {
            return Err(refuse(ErrorCode::Invalid, format!("bad space name {name:?}")));
        }
        if self.find(name, &SpacePath::root())?.is_some() {
            return Err(refuse(ErrorCode::Exists, format!("space {name} exists")));
        }
        Ok(())
    }
}

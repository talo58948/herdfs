//! Namespace requests: stat, list, create, remove, rename and attributes.

use herdfs_core::{
    SpacePath,
    proto::{Entry, EntryId, ErrorCode, Kind, Perm, Response},
};
use rusqlite::params;

use crate::{
    Call, Result,
    db::{below, kind_name, parse},
    refuse,
};

/// Symlinks report this mode, as on Linux.
pub(crate) const SYMLINK_MODE: u32 = 0o777;

impl Call<'_> {
    pub(crate) fn stat(&self, space: &str, path: &SpacePath) -> Result<Response> {
        // Any grant in the space shows its root, so a principal can mount above what it may read.
        if path.is_root() {
            self.require_somewhere(space, Perm::Read)?;
        } else {
            self.require(space, path, Perm::Read)?;
        }
        Ok(Response::Entry {
            entry: self.entry(space, path)?,
        })
    }

    /// The entry at its current path, so a client can follow renames made elsewhere.
    pub(crate) fn locate(&self, space: &str, entry: EntryId) -> Result<Response> {
        let entry = self.entry_by_id(space, entry)?;
        self.require(space, &entry.path, Perm::Read)?;
        Ok(Response::Entry { entry })
    }

    pub(crate) fn list(&self, space: &str, path: &SpacePath) -> Result<Response> {
        self.require(space, path, Perm::Read)?;
        if self.entry(space, path)?.kind != Kind::Dir {
            return Err(refuse(ErrorCode::NotDir, format!("{path} is not a directory")));
        }
        Ok(Response::Entries {
            entries: self.children(space, path)?,
        })
    }

    /// Creates a file, directory or symlink.
    pub(crate) fn make(
        &self,
        space: &str,
        parent: EntryId,
        name: &str,
        kind: Kind,
        mode: u32,
        target: Option<&str>,
    ) -> Result<Response> {
        let (path, parent_kind) = self.child_path(space, parent, name)?;
        self.require(space, &path, Perm::Write)?;
        self.require_directory(parent_kind, parent)?;
        if self.find(space, &path)?.is_some() {
            return Err(refuse(ErrorCode::Exists, format!("{path} exists")));
        }
        self.db
            .prepare_cached(
                "INSERT INTO entries (space, path, kind, mode, mtime_ms, target) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![space, path.as_str(), kind_name(kind), mode, self.now_ms, target])?;
        self.touch_parent(space, &path)?;
        Ok(Response::Entry {
            entry: self.entry(space, &path)?,
        })
    }

    pub(crate) fn remove(&self, space: &str, parent: EntryId, name: &str) -> Result<Response> {
        let (path, parent_kind) = self.child_path(space, parent, name)?;
        self.require(space, &path, Perm::Write)?;
        self.require_directory(parent_kind, parent)?;
        let entry = self.entry(space, &path)?;
        self.delete(space, &entry)?;
        self.touch_parent(space, &path)?;
        Ok(Response::Entry { entry })
    }

    pub(crate) fn rename(
        &self,
        space: &str,
        from_parent: EntryId,
        from_name: &str,
        to_parent: EntryId,
        to_name: &str,
    ) -> Result<Response> {
        let (from, from_kind) = self.child_path(space, from_parent, from_name)?;
        let (to, to_kind) = self.child_path(space, to_parent, to_name)?;
        self.require(space, &from, Perm::Write)?;
        self.require(space, &to, Perm::Write)?;
        self.require_directory(from_kind, from_parent)?;
        self.require_directory(to_kind, to_parent)?;
        let source = self.entry(space, &from)?;
        if from == to {
            return Ok(Response::Renamed { from, to });
        }
        if to.is_within(&from) {
            return Err(refuse(
                ErrorCode::Invalid,
                format!("cannot move {from} into itself"),
            ));
        }
        // Every path below `from` must stay within the path limit once moved.
        if self.longest_within(space, &from)?.rebase(&from, &to).is_none() {
            let message = format!("moving {from} to {to} makes a path too long");
            return Err(refuse(ErrorCode::Invalid, message));
        }
        if let Some(target) = self.find(space, &to)? {
            self.replace(space, &source, &target)?;
        }
        // `from` is not the root here, since everything is within the root.
        let (lo, hi) = below(&from);
        self.db
            .prepare_cached(
                "UPDATE entries SET path = ?3 || substr(path, length(?2) + 1)
                 WHERE space = ?1 AND (path = ?2 OR (path > ?4 AND path < ?5))",
            )?
            .execute(params![space, from.as_str(), to.as_str(), lo, hi])?;
        self.touch_parent(space, &from)?;
        self.touch_parent(space, &to)?;
        Ok(Response::Renamed { from, to })
    }

    pub(crate) fn set_attr(
        &self,
        space: &str,
        path: &SpacePath,
        mode: Option<u32>,
        mtime_ms: Option<i64>,
    ) -> Result<Response> {
        self.require(space, path, Perm::Write)?;
        let entry = self.entry(space, path)?;
        let entry = Entry {
            mode: mode.unwrap_or(entry.mode),
            mtime_ms: mtime_ms.unwrap_or(entry.mtime_ms),
            ..entry
        };
        self.db
            .prepare_cached("UPDATE entries SET mode = ?2, mtime_ms = ?3 WHERE id = ?1")?
            .execute(params![entry.id.0, entry.mode, entry.mtime_ms])?;
        Ok(Response::Entry { entry })
    }

    /// Removes `target` so that `source` can take its place.
    fn replace(&self, space: &str, source: &Entry, target: &Entry) -> Result<()> {
        match (source.kind == Kind::Dir, target.kind == Kind::Dir) {
            (true, false) => Err(refuse(
                ErrorCode::NotDir,
                format!("{} is not a directory", target.path),
            )),
            (false, true) => Err(refuse(
                ErrorCode::IsDir,
                format!("{} is a directory", target.path),
            )),
            _ => self.delete(space, target),
        }
    }

    /// Deletes a file, a symlink or an empty directory, with its versions.
    fn delete(&self, space: &str, entry: &Entry) -> Result<()> {
        if entry.kind == Kind::Dir && self.has_children(space, &entry.path)? {
            return Err(refuse(
                ErrorCode::NotEmpty,
                format!("{} is not empty", entry.path),
            ));
        }
        self.db
            .prepare_cached("DELETE FROM entries WHERE id = ?1")?
            .execute([entry.id.0])?;
        Ok(())
    }

    /// The longest path, in bytes, at or below the non-root `dir`.
    fn longest_within(&self, space: &str, dir: &SpacePath) -> Result<SpacePath> {
        let (lo, hi) = below(dir);
        let mut query = self.db.prepare_cached(
            "SELECT path FROM entries WHERE space = ?1 AND (path = ?2 OR (path > ?3 AND path < ?4))
             ORDER BY length(CAST(path AS BLOB)) DESC LIMIT 1",
        )?;
        let longest = query.query_row(params![space, dir.as_str(), lo, hi], |row| {
            parse(row, 0, |text| SpacePath::try_from(text).ok())
        })?;
        Ok(longest)
    }

    /// A directory's mtime moves when an entry is added to it or taken from it.
    fn touch_parent(&self, space: &str, path: &SpacePath) -> Result<()> {
        let parent = path.parent().unwrap_or_default();
        self.db
            .prepare_cached("UPDATE entries SET mtime_ms = ?3 WHERE space = ?1 AND path = ?2")?
            .execute(params![space, parent.as_str(), self.now_ms])?;
        Ok(())
    }

    fn child_path(&self, space: &str, parent: EntryId, name: &str) -> Result<(SpacePath, Kind)> {
        let parent_entry = self.entry_by_id(space, parent)?;
        let name = SpacePath::try_from(name)
            .ok()
            .filter(|name| name.parent() == Some(SpacePath::root()))
            .ok_or_else(|| refuse(ErrorCode::Invalid, format!("invalid name {name:?}")))?;
        let Ok(path) = parent_entry.path.join(name.as_str()) else {
            self.require(space, &parent_entry.path, Perm::Write)?;
            return Err(refuse(
                ErrorCode::Invalid,
                format!("path below entry {} is too long", parent.0),
            ));
        };
        Ok((path, parent_entry.kind))
    }

    fn require_directory(&self, kind: Kind, parent: EntryId) -> Result<()> {
        if kind == Kind::Dir {
            Ok(())
        } else {
            Err(refuse(
                ErrorCode::NotFound,
                format!("entry {} is not a directory", parent.0),
            ))
        }
    }
}

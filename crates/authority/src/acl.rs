//! Permissions: the effective perm of a caller on a path, and `SetAcl`.

use herdfs_core::{
    SpacePath,
    proto::{AclRow, ErrorCode, Perm, Response},
};
use rusqlite::params;

use crate::{Call, Fail, Result, db::parse, refuse};

pub(crate) fn perm_name(perm: Perm) -> &'static str {
    match perm {
        Perm::Read => "read",
        Perm::Write => "write",
        Perm::Admin => "admin",
    }
}

pub(crate) fn perm_from_name(name: &str) -> Option<Perm> {
    [Perm::Read, Perm::Write, Perm::Admin]
        .into_iter()
        .find(|perm| perm_name(*perm) == name)
}

/// One ACL row of the caller.
struct Grant {
    path: SpacePath,
    perm: Perm,
}

/// One access interval, ordered by ACL changes rather than file commits.
struct Access {
    path: SpacePath,
    granted_seq: i64,
    revoked_seq: Option<i64>,
    version_floor: i64,
}

/// Names only the perm: the path of a request by id may be hidden from the caller.
fn denied(need: Perm) -> Fail {
    refuse(ErrorCode::Denied, format!("denied: needs {}", perm_name(need)))
}

impl Call<'_> {
    /// Fails with `NotFound` for a missing space and `Denied` below `need`,
    /// whether or not `path` exists.
    pub(crate) fn require(&self, space: &str, path: &SpacePath, need: Perm) -> Result<()> {
        self.require_space(space)?;
        if !self.may(space, path, need)? {
            return Err(denied(need));
        }
        Ok(())
    }

    /// Whether the caller holds `need` on `path`: its highest perm there or on an ancestor.
    pub(crate) fn may(&self, space: &str, path: &SpacePath, need: Perm) -> Result<bool> {
        let grants = self.grants(space)?.into_iter();
        let best = grants
            .filter(|grant| path.is_within(&grant.path))
            .map(|grant| grant.perm)
            .max();
        Ok(best >= Some(need))
    }

    /// The commit sequence after which the caller could first read `path`, so
    /// it sees no history from before (S8). Admins of the root see it all.
    pub(crate) fn readable_after(&self, space: &str, path: &SpacePath) -> Result<i64> {
        let grants = self.grants(space)?;
        if grants
            .iter()
            .any(|grant| grant.path.is_root() && grant.perm == Perm::Admin)
        {
            return Ok(0);
        }
        let accesses = self
            .accesses(space)?
            .into_iter()
            .filter(|access| path.is_within(&access.path))
            .collect::<Vec<_>>();
        let mut start = i64::MAX;
        let mut floor = i64::MAX;
        loop {
            let previous = start;
            for access in &accesses {
                if access.granted_seq <= start
                    && access.revoked_seq.is_none_or(|end| end > start)
                {
                    start = start.min(access.granted_seq);
                    floor = floor.min(access.version_floor);
                }
            }
            if start == previous {
                return Ok(floor);
            }
        }
    }

    /// Fails unless the caller holds `need` on some path of the space.
    pub(crate) fn require_somewhere(&self, space: &str, need: Perm) -> Result<()> {
        self.require_space(space)?;
        if !self.grants(space)?.iter().any(|grant| grant.perm >= need) {
            return Err(denied(need));
        }
        Ok(())
    }

    /// The caller's ACL rows in `space`, from its user and its groups.
    fn grants(&self, space: &str) -> Result<Vec<Grant>> {
        let mut query = self
            .db
            .prepare_cached("SELECT path, perm FROM acl WHERE space = ?1 AND principal = ?2")?;
        let mut grants = Vec::new();
        for principal in self.principals() {
            let rows = query.query_map(params![space, principal], |row| {
                let granted = parse(row, 0, |text| SpacePath::try_from(text).ok())?;
                let perm = parse(row, 1, perm_from_name)?;
                Ok(Grant { path: granted, perm })
            })?;
            grants.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(grants)
    }

    /// Past read access for the caller's user and current token groups.
    fn accesses(&self, space: &str) -> Result<Vec<Access>> {
        let mut query = self.db.prepare_cached(
            "SELECT path, seq, revoked_seq, granted_version FROM acl_history
             WHERE space = ?1 AND principal = ?2",
        )?;
        let mut accesses = Vec::new();
        for principal in self.principals() {
            let rows = query.query_map(params![space, principal], |row| {
                Ok(Access {
                    path: parse(row, 0, |text| SpacePath::try_from(text).ok())?,
                    granted_seq: row.get(1)?,
                    revoked_seq: row.get(2)?,
                    version_floor: row.get(3)?,
                })
            })?;
            accesses.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(accesses)
    }

    pub(crate) fn set_acl(
        &self,
        space: &str,
        path: &SpacePath,
        principal: &str,
        perm: Option<Perm>,
    ) -> Result<Response> {
        self.require(space, path, Perm::Admin)?;
        let name = principal
            .strip_prefix("user:")
            .or_else(|| principal.strip_prefix("group:"));
        if name.is_none_or(str::is_empty) {
            return Err(refuse(ErrorCode::Invalid, format!("bad principal {principal:?}")));
        }
        match perm {
            Some(perm) => {
                let inserted = self
                    .db
                    .prepare_cached(
                        "INSERT OR IGNORE INTO acl (space, path, principal, perm, granted_seq)
                         VALUES (?1, ?2, ?3, ?4, coalesce((SELECT max(id) FROM versions), 0))",
                    )?
                    .execute(params![space, path.as_str(), principal, perm_name(perm)])?;
                self.db
                    .prepare_cached(
                        "UPDATE acl SET perm = ?4 WHERE space = ?1 AND path = ?2 AND principal = ?3",
                    )?
                    .execute(params![space, path.as_str(), principal, perm_name(perm)])?;
                if inserted != 0 {
                    self.db
                        .prepare_cached(
                            "INSERT INTO acl_history (space, path, principal, granted_version)
                             SELECT space, path, principal, granted_seq FROM acl
                             WHERE space = ?1 AND path = ?2 AND principal = ?3",
                        )?
                        .execute(params![space, path.as_str(), principal])?;
                }
            }
            None => {
                let removed = self
                    .db
                    .prepare_cached(
                        "DELETE FROM acl WHERE space = ?1 AND path = ?2 AND principal = ?3",
                    )?
                    .execute(params![space, path.as_str(), principal])?;
                if removed != 0 {
                    self.db
                        .prepare_cached(
                            "UPDATE acl_history
                             SET revoked_seq = (SELECT coalesce(max(seq), 0) + 1 FROM acl_history)
                             WHERE space = ?1 AND path = ?2 AND principal = ?3 AND revoked_seq IS NULL",
                        )?
                        .execute(params![space, path.as_str(), principal])?;
                }
            }
        };
        Ok(Response::Done)
    }

    pub(crate) fn access(&self, space: &str, path: &SpacePath) -> Result<Response> {
        self.require(space, path, Perm::Admin)?;
        let mut query = self.db.prepare_cached(
            "SELECT path, principal, perm FROM acl WHERE space = ?1
             ORDER BY length(path), path, principal",
        )?;
        let rows = query.query_map([space], |row| {
            let grant_path = parse(row, 0, |text| SpacePath::try_from(text).ok())?;
            let perm = parse(row, 2, perm_from_name)?;
            Ok(AclRow {
                path: grant_path,
                principal: row.get(1)?,
                perm,
            })
        })?;
        let grants = rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|grant| path.is_within(&grant.path))
            .collect();
        Ok(Response::Access { grants })
    }

    /// Makes the caller admin of the root of `space`.
    pub(crate) fn make_admin(&self, space: &str) -> Result<()> {
        self.db
            .prepare_cached(
                "INSERT INTO acl (space, path, principal, perm, granted_seq)
                 VALUES (?1, '', ?2, ?3, coalesce((SELECT max(id) FROM versions), 0))",
            )?
            .execute(params![space, self.who.user, perm_name(Perm::Admin)])?;
        self.db
            .prepare_cached(
                "INSERT INTO acl_history (space, path, principal, granted_version)
                 SELECT space, path, principal, granted_seq FROM acl
                 WHERE space = ?1 AND path = '' AND principal = ?2",
            )?
            .execute(params![space, self.who.user])?;
        Ok(())
    }

    /// The caller's user and group principals, as ACL rows name them.
    pub(crate) fn principals(&self) -> impl Iterator<Item = String> + '_ {
        let groups = self.who.groups.iter().map(|group| format!("group:{group}"));
        std::iter::once(self.who.user.clone()).chain(groups)
    }
}

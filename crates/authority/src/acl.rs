//! Permissions: the effective perm of a caller on a path, and `SetAcl`.

use herdfs_core::{
    SpacePath,
    proto::{ErrorCode, Perm, Response},
};
use rusqlite::params;

use crate::{Call, Fail, Result, db::parse, refuse};

fn perm_name(perm: Perm) -> &'static str {
    match perm {
        Perm::Read => "read",
        Perm::Write => "write",
        Perm::Admin => "admin",
    }
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
        let grants = self.grants(space)?.into_iter();
        let best = grants
            .filter(|(granted, _)| path.is_within(granted))
            .map(|(_, perm)| perm)
            .max();
        if best < Some(need) {
            return Err(denied(need));
        }
        Ok(())
    }

    /// Fails unless the caller holds `need` on some path of the space.
    pub(crate) fn require_somewhere(&self, space: &str, need: Perm) -> Result<()> {
        self.require_space(space)?;
        if !self.grants(space)?.iter().any(|(_, perm)| *perm >= need) {
            return Err(denied(need));
        }
        Ok(())
    }

    /// The caller's ACL rows in `space`, from its user and its groups.
    fn grants(&self, space: &str) -> Result<Vec<(SpacePath, Perm)>> {
        let mut query = self
            .db
            .prepare_cached("SELECT path, perm FROM acl WHERE space = ?1 AND principal = ?2")?;
        let mut grants = Vec::new();
        for principal in self.principals() {
            let rows = query.query_map(params![space, principal], |row| {
                let granted = parse(row, 0, |text| SpacePath::try_from(text).ok())?;
                let perm = parse(row, 1, |text| {
                    [Perm::Read, Perm::Write, Perm::Admin]
                        .into_iter()
                        .find(|perm| perm_name(*perm) == text)
                })?;
                Ok((granted, perm))
            })?;
            grants.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(grants)
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
            Some(perm) => self
                .db
                .prepare_cached(
                    "INSERT OR REPLACE INTO acl (space, path, principal, perm) VALUES (?1, ?2, ?3, ?4)",
                )?
                .execute(params![space, path.as_str(), principal, perm_name(perm)])?,
            None => self
                .db
                .prepare_cached("DELETE FROM acl WHERE space = ?1 AND path = ?2 AND principal = ?3")?
                .execute(params![space, path.as_str(), principal])?,
        };
        Ok(Response::Done)
    }

    /// Makes the caller admin of the root of `space`.
    pub(crate) fn make_admin(&self, space: &str) -> Result<()> {
        self.db
            .prepare_cached("INSERT INTO acl (space, path, principal, perm) VALUES (?1, '', ?2, ?3)")?
            .execute(params![space, self.who.user, perm_name(Perm::Admin)])?;
        Ok(())
    }

    /// The caller's user and group principals, as ACL rows name them.
    fn principals(&self) -> impl Iterator<Item = String> + '_ {
        let groups = self.who.groups.iter().map(|group| format!("group:{group}"));
        std::iter::once(self.who.user.clone()).chain(groups)
    }
}

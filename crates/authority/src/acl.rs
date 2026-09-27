//! Permissions: the effective perm of a caller on a path, and `SetAcl`.

use herdfs_core::{
    SpacePath,
    proto::{ErrorCode, Perm, Response},
};
use rusqlite::params;

use crate::{Call, Result, db::parse, refuse};

fn perm_name(perm: Perm) -> &'static str {
    match perm {
        Perm::Read => "read",
        Perm::Write => "write",
        Perm::Admin => "admin",
    }
}

impl Call<'_> {
    /// Fails with `NotFound` for a missing space and `Denied` below `need`,
    /// whether or not `path` exists.
    pub(crate) fn require(&self, space: &str, path: &SpacePath, need: Perm) -> Result<()> {
        self.require_space(space)?;
        if self.perm(space, path)? >= Some(need) {
            return Ok(());
        }
        let message = format!("{} lacks {} on {path}", self.who.user, perm_name(need));
        Err(refuse(ErrorCode::Denied, message))
    }

    /// The highest perm granted to the caller, or one of its groups, on
    /// `path` or any ancestor.
    fn perm(&self, space: &str, path: &SpacePath) -> Result<Option<Perm>> {
        let mut query = self
            .db
            .prepare_cached("SELECT path, perm FROM acl WHERE space = ?1 AND principal = ?2")?;
        let mut best = None;
        for principal in self.principals() {
            let rows = query.query_map(params![space, principal], |row| {
                let granted = parse(row, 0, |text| SpacePath::try_from(text).ok())?;
                let perm = parse(row, 1, |text| {
                    [Perm::Read, Perm::Write, Perm::Admin]
                        .into_iter()
                        .find(|perm| perm_name(*perm) == text)
                })?;
                Ok(path.is_within(&granted).then_some(perm))
            })?;
            for perm in rows {
                best = best.max(perm?);
            }
        }
        Ok(best)
    }

    /// Uploads need write somewhere in the space: they are only useful to commit.
    pub(crate) fn may_upload(&self, space: &str) -> Result<()> {
        self.require_space(space)?;
        let mut query = self.db.prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM acl WHERE space = ?1 AND principal = ?2 AND perm IN ('write', 'admin'))",
        )?;
        for principal in self.principals() {
            if query.query_row(params![space, principal], |row| row.get(0))? {
                return Ok(());
            }
        }
        Err(refuse(
            ErrorCode::Denied,
            format!("{} cannot write in {space}", self.who.user),
        ))
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

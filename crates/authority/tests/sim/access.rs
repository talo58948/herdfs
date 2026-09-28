//! Who may do what in the model: permissions, and S8's history since grant.

use herdfs_authority::Principal;
use herdfs_core::{
    SpacePath,
    proto::{ErrorCode, Perm, VersionId},
};

use crate::model::{Model, Node, Outcome, ensure};

impl Model {
    /// `who`'s rows in `space`: path, perm and grant time.
    fn rows<'a>(
        &'a self,
        space: &str,
        who: &'a Principal,
    ) -> impl Iterator<Item = (&'a SpacePath, Perm, i64)> {
        let mine = move |principal: &String| {
            *principal == who.user
                || who
                    .groups
                    .iter()
                    .any(|group| *principal == format!("group:{group}"))
        };
        let acl = self
            .spaces
            .get(space)
            .map(|space| &space.acl)
            .into_iter()
            .flatten();
        acl.filter(move |((_, principal), _)| mine(principal))
            .map(|((at, _), (perm, granted))| (at, *perm, *granted))
    }

    pub fn perm(&self, space: &str, path: &SpacePath, who: &Principal) -> Option<Perm> {
        self.rows(space, who)
            .filter(|(at, ..)| path.is_within(at))
            .map(|(_, perm, _)| perm)
            .max()
    }

    /// S8: when `who` could first read `path`; admins of the root see all history.
    pub fn readable_since(&self, space: &str, path: &SpacePath, who: &Principal) -> i64 {
        let mut rows = self.rows(space, who);
        if rows.any(|(at, perm, _)| at.is_root() && perm == Perm::Admin) {
            return i64::MIN;
        }
        let applying = self.rows(space, who).filter(|(at, ..)| path.is_within(at));
        applying.map(|(.., granted)| granted).min().unwrap_or(i64::MAX)
    }

    /// Whether `who` may see `version` of the node at `path`: its head, or one made since access began.
    pub fn visible(
        &self,
        space: &str,
        path: &SpacePath,
        node: &Node,
        version: VersionId,
        who: &Principal,
    ) -> bool {
        node.history.last() == Some(&version)
            || self.versions[&version].created_ms >= self.readable_since(space, path, who)
    }

    pub fn require(&self, who: &Principal, space: &str, path: &SpacePath, need: Perm) -> Outcome {
        self.space(space)?;
        ensure(self.perm(space, path, who) >= Some(need), ErrorCode::Denied)
    }

    /// Uploads need write on some path of the space.
    pub fn upload_outcome(&self, who: &Principal, space: &str) -> Outcome {
        self.require_somewhere(who, space, Perm::Write)
    }

    pub fn require_somewhere(&self, who: &Principal, space: &str, need: Perm) -> Outcome {
        self.space(space)?;
        ensure(
            self.rows(space, who).any(|(_, perm, _)| perm >= need),
            ErrorCode::Denied,
        )
    }
}

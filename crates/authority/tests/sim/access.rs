//! Who may do what in the model: permissions, and S8's history since grant.

use herdfs_authority::Principal;
use herdfs_core::{
    SpacePath,
    proto::{ErrorCode, Perm, VersionId},
};

use crate::model::{Model, Node, Outcome, ensure};

/// One modeled interval of read access from an ACL row.
#[derive(Clone)]
pub struct Access {
    path: SpacePath,
    principal: String,
    granted_seq: u64,
    revoked_seq: Option<u64>,
    version_floor: u64,
}

fn mine(principal: &str, who: &Principal) -> bool {
    principal == who.user
        || principal
            .strip_prefix("group:")
            .is_some_and(|group| who.groups.iter().any(|mine| mine == group))
}

impl Model {
    /// `who`'s rows in `space`: path, perm and grant sequence.
    fn rows<'a>(
        &'a self,
        space: &str,
        who: &'a Principal,
    ) -> impl Iterator<Item = (&'a SpacePath, Perm, u64)> {
        let acl = self
            .spaces
            .get(space)
            .map(|space| &space.acl)
            .into_iter()
            .flatten();
        acl.filter(move |((_, principal), _)| mine(principal, who))
            .map(|((at, _), (perm, granted))| (at, *perm, *granted))
    }

    pub fn perm(&self, space: &str, path: &SpacePath, who: &Principal) -> Option<Perm> {
        self.rows(space, who)
            .filter(|(at, ..)| path.is_within(at))
            .map(|(_, perm, _)| perm)
            .max()
    }

    /// S8: the sequence after which `who` could first read `path`; root admins see all history.
    pub fn readable_after(&self, space: &str, path: &SpacePath, who: &Principal) -> u64 {
        let mut rows = self.rows(space, who);
        if rows.any(|(at, perm, _)| at.is_root() && perm == Perm::Admin) {
            return 0;
        }
        let Some(space) = self.spaces.get(space) else {
            return u64::MAX;
        };
        let accesses: Vec<_> = space
            .access
            .iter()
            .filter(|access| mine(&access.principal, who) && path.is_within(&access.path))
            .collect();
        let mut start = u64::MAX;
        let mut floor = u64::MAX;
        loop {
            let previous = start;
            for access in &accesses {
                if access.granted_seq <= start && access.revoked_seq.is_none_or(|end| end > start) {
                    start = start.min(access.granted_seq);
                    floor = floor.min(access.version_floor);
                }
            }
            if start == previous {
                return floor;
            }
        }
    }

    /// Whether `who` may see `version` of the node at `path`: its head, one made since access
    /// began, or one `who` authored.
    pub fn visible(
        &self,
        space: &str,
        path: &SpacePath,
        node: &Node,
        version: VersionId,
        who: &Principal,
    ) -> bool {
        node.history.last() == Some(&version)
            || version.0 > self.readable_after(space, path, who)
            || self.versions[&version].author == who.user
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

    /// Applies one successful ACL mutation to current permissions and interval history.
    pub fn change_acl(&mut self, space: &str, path: &SpacePath, principal: &str, perm: Option<Perm>) {
        let key = (path.clone(), principal.to_owned());
        let exists = self.spaces[space].acl.contains_key(&key);
        if let (true, Some(perm)) = (exists, perm) {
            self.spaces
                .get_mut(space)
                .expect("space exists")
                .acl
                .get_mut(&key)
                .expect("row exists")
                .0 = perm;
            return;
        }
        if !exists && perm.is_none() {
            return;
        }
        self.acl_seq += 1;
        let event = self.acl_seq;
        let version_floor = self.versions.keys().next_back().map_or(0, |id| id.0);
        let space = self.spaces.get_mut(space).expect("space exists");
        match perm {
            Some(perm) => {
                space.acl.insert(key, (perm, version_floor));
                space.access.push(Access {
                    path: path.clone(),
                    principal: principal.to_owned(),
                    granted_seq: event,
                    revoked_seq: None,
                    version_floor,
                });
            }
            None => {
                let _ = space.acl.remove(&key);
                let active = space
                    .access
                    .iter_mut()
                    .find(|access| {
                        access.path == *path && access.principal == principal && access.revoked_seq.is_none()
                    })
                    .expect("current ACL row has an active interval");
                active.revoked_seq = Some(event);
            }
        }
    }
}

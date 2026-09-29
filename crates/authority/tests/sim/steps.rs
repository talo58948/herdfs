//! Steps on the namespace, ACLs and reads. Each picks a request, sends it
//! through `Sim::exec`, and runs the targeted checks its invariant needs.

use herdfs_core::{
    SpacePath,
    proto::{Perm, Request},
};

use crate::{GRANTEES, NO_ENTRY, SPACES, Sim};

impl Sim {
    pub fn new_space(&mut self) {
        let who = self.actor();
        let name = SPACES[self.rng.below(SPACES.len())].to_owned();
        let request = match self.rng.chance(50) {
            true => Request::CreateSpace { space: name.clone() },
            false => Request::Fork {
                space: self.space(),
                name: name.clone(),
            },
        };
        if self.exec(who, &request).is_ok() {
            self.let_auditor_in(&name);
        }
    }

    pub fn make(&mut self) {
        // Mostly alice, who can write, so that trees grow deep enough to move.
        let who = if self.rng.chance(40) { 0 } else { self.actor() };
        let space = self.space();
        let path = self.path(&space);
        let parent = self.parent(&space, &path);
        let request = match self.rng.below(20) {
            0..=9 => Request::Create {
                space,
                parent,
                name: path.name().into(),
                mode: 0o644,
            },
            10..=16 => Request::Mkdir {
                space,
                parent,
                name: path.name().into(),
                mode: 0o755,
            },
            _ => Request::Symlink {
                space,
                parent,
                name: path.name().into(),
                target: "../elsewhere".into(),
            },
        };
        self.send(who, &request);
    }

    pub fn remove(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let path = self.existing_path(&space);
        let parent = self.parent(&space, &path);
        self.send(
            who,
            &Request::Remove {
                space,
                parent,
                name: path.name().into(),
            },
        );
    }

    pub fn rename(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let (from, to) = (self.existing_path(&space), self.path(&space));
        let moved: Vec<_> = self.model.spaces.get(&space).map_or_else(Vec::new, |known| {
            let within = known.nodes.iter().filter(|(path, _)| path.is_within(&from));
            within.map(|(path, node)| (path.clone(), node.id)).collect()
        });
        let request = Request::Rename {
            space: space.clone(),
            from_parent: self.parent(&space, &from),
            from_name: from.name().into(),
            to_parent: self.parent(&space, &to),
            to_name: to.name().into(),
        };
        if self.exec(who, &request).is_ok() && from != to {
            self.check_rename(&space, &from, &to, &moved);
        }
    }

    pub(super) fn parent(&self, space: &str, child: &SpacePath) -> herdfs_core::proto::EntryId {
        let parent = child.parent().unwrap_or_default();
        self.model.node(space, &parent).map_or(NO_ENTRY, |node| node.id)
    }
    pub fn set_attr(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let path = self.existing_path(&space);
        let entry = self.model.node(&space, &path).map_or(NO_ENTRY, |node| node.id);
        let mode = self.rng.chance(50).then_some(0o600);
        let mtime_ms = self.rng.chance(50).then_some(self.rng.below(1 << 30) as i64);
        self.send(
            who,
            &Request::SetAttr {
                space,
                entry,
                mode,
                mtime_ms,
            },
        );
    }
    pub fn purge(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let path = self.existing_path(&space);
        self.send(who, &Request::Purge { space, path });
    }
    /// Deletes a space; space `s` comes back at once, empty, so the run goes on.
    pub fn delete_space(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let deleted = self
            .exec(who, &Request::DeleteSpace { space: space.clone() })
            .is_ok();
        if deleted && space == "s" {
            self.exec(0, &Request::CreateSpace { space }).unwrap();
            self.let_auditor_in("s");
        }
    }

    pub fn set_acl(&mut self) {
        let who = if self.rng.chance(70) { 0 } else { self.actor() };
        let space = self.space();
        let path = if self.rng.chance(25) {
            SpacePath::root()
        } else {
            self.existing_path(&space)
        };
        let principal = GRANTEES[self.rng.below(GRANTEES.len())].to_owned();
        let perm = [None, None, Some(Perm::Read), Some(Perm::Write), Some(Perm::Admin)][self.rng.below(5)];
        let request = Request::SetAcl {
            space: space.clone(),
            path: path.clone(),
            principal,
            perm,
        };
        if self.exec(who, &request).is_ok() && perm.is_none() {
            self.check_revocation(&space, &path);
        }
    }

    pub fn read(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let path = self.existing_path(&space);
        let entry = self.model.node(&space, &path).map_or(NO_ENTRY, |node| node.id);
        let parent = self.parent(&space, &path);
        let request = match self.rng.below(7) {
            0 => Request::Stat { space, path },
            1 => Request::List { space, entry },
            2 => Request::Open { space, entry },
            3 => Request::History { space, path },
            4 => Request::Locate { space, entry },
            5 => Request::Lookup {
                space,
                parent,
                name: path.name().into(),
            },
            _ => Request::Whoami,
        };
        self.send(who, &request);
    }
}

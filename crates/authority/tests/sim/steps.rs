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

    /// Builds a chain of directories whose deepest path nears the 4096-byte
    /// limit, then renames its top to a longer or a shorter name. The model
    /// refuses a rename that pushes a path past the limit.
    pub fn deep_rename(&mut self) {
        let space = self.space();
        let (top, long) = (SpacePath::try_from("z").unwrap(), "d".repeat(255));
        let mut deepest = top.clone();
        let mut mkdir = Request::Mkdir {
            space: space.clone(),
            path: deepest.clone(),
            parent: self.parent(&space, &deepest),
            mode: 0o755,
        };
        self.send(0, &mkdir);
        for _ in 0..15 {
            deepest = deepest.join(&long).unwrap();
            mkdir = Request::Mkdir {
                space: space.clone(),
                path: deepest.clone(),
                parent: self.parent(&space, &deepest),
                mode: 0o755,
            };
            self.send(0, &mkdir);
        }
        // 1 + 15 * 256 + 201 = 4042 bytes.
        let file = deepest.join(&"f".repeat(200)).unwrap();
        self.send(
            0,
            &Request::Create {
                space: space.clone(),
                parent: self.parent(&space, &file),
                path: file,
                mode: 0o644,
            },
        );
        let to = if self.rng.chance(50) {
            "e".repeat(255)
        } else {
            "short".into()
        };
        let to = SpacePath::try_from(to).unwrap();
        // contract: C7
        let rename = Request::Rename {
            space: space.clone(),
            from: top.clone(),
            from_parent: self.parent(&space, &top),
            to: to.clone(),
            to_parent: self.parent(&space, &to),
        };
        self.send(0, &rename);
        // Take the chain down again, deepest first: every later check would walk it.
        let known = self.model.spaces.get(&space).map(|known| known.nodes.keys());
        let chain = known
            .into_iter()
            .flatten()
            .filter(|path| path.is_within(&top) || path.is_within(&to));
        let mut chain: Vec<_> = chain.cloned().collect();
        chain.sort_by_key(|path| std::cmp::Reverse(path.as_str().len()));
        for path in chain {
            let parent = self.parent(&space, &path);
            self.send(
                0,
                &Request::Remove {
                    space: space.clone(),
                    path,
                    parent,
                },
            );
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
                path,
                parent,
                mode: 0o644,
            },
            10..=16 => Request::Mkdir {
                space,
                path,
                parent,
                mode: 0o755,
            },
            _ => Request::Symlink {
                space,
                path,
                parent,
                target: "../elsewhere".into(),
            },
        };
        self.send(who, &request);
    }

    pub fn remove(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let path = self.existing_path(&space);
        let parent = self.parent(&space, &path);
        self.send(who, &Request::Remove { space, path, parent });
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
            from: from.clone(),
            from_parent: self.parent(&space, &from),
            to: to.clone(),
            to_parent: self.parent(&space, &to),
        };
        if self.exec(who, &request).is_ok() && from != to {
            self.check_rename(&space, &from, &to, &moved);
        }
    }

    fn parent(&self, space: &str, child: &SpacePath) -> herdfs_core::proto::EntryId {
        let parent = child.parent().unwrap_or_default();
        self.model.node(space, &parent).map_or(NO_ENTRY, |node| node.id)
    }

    pub fn set_attr(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let path = self.existing_path(&space);
        let mode = self.rng.chance(50).then_some(0o600);
        let mtime_ms = self.rng.chance(50).then_some(self.rng.below(1 << 30) as i64);
        self.send(
            who,
            &Request::SetAttr {
                space,
                path,
                mode,
                mtime_ms,
            },
        );
    }

    /// Grants a user read or write on a file with history, then has them read
    /// it and commit on its oldest version reusing its chunks: versions from
    /// before the grant stay hidden and unusable (S6, S8).
    pub fn late_reader(&mut self) {
        let space = self.space();
        let files = self.model.spaces.get(&space).map(|known| known.nodes.iter());
        let files = files
            .into_iter()
            .flatten()
            .filter(|(_, node)| node.history.len() > 1);
        let files: Vec<_> = files
            .map(|(path, node)| (path.clone(), node.id, node.history[0]))
            .collect();
        let Some((path, entry, oldest)) = self.rng.pick(&files) else {
            return;
        };
        let (who, principal) = [(1, "user:bob"), (2, "user:carol"), (2, "group:eng")][self.rng.below(3)];
        let perm = Some(if self.rng.chance(50) {
            Perm::Write
        } else {
            Perm::Read
        });
        let (space, principal) = (space.clone(), principal.to_owned());
        self.send(
            0,
            &Request::SetAcl {
                space: space.clone(),
                path: path.clone(),
                principal,
                perm,
            },
        );
        self.send(
            who,
            &Request::History {
                space: space.clone(),
                path,
            },
        );
        self.send(
            who,
            &Request::Grant {
                space: space.clone(),
                entry,
                version: oldest,
                chunks: vec![0],
            },
        );
        let chunks = self.model.versions[&oldest].chunks.clone();
        let request_id = format!("r{}", self.sent.len());
        let commit = Request::Commit {
            space,
            entry,
            request_id,
            base: Some(oldest),
            guarded: false,
            chunks,
            mtime_ms: None,
        };
        self.sent.push((who, commit.clone()));
        self.send(who, &commit);
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
        let request = match self.rng.below(6) {
            0 => Request::Stat { space, path },
            1 => Request::List { space, path },
            2 => Request::Open { space, path },
            3 => Request::History { space, path },
            4 => Request::Locate { space, entry },
            _ => Request::Whoami,
        };
        self.send(who, &request);
    }
}

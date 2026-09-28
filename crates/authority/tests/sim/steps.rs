//! Steps on the namespace, ACLs and reads. Each picks a request, sends it
//! through `Sim::exec`, and runs the targeted checks its invariant needs.

use herdfs_core::{
    SpacePath,
    proto::{Perm, Request},
};

use crate::{GRANTEES, SPACES, Sim};

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
        let mkdir = |path: &SpacePath| Request::Mkdir {
            space: space.clone(),
            path: path.clone(),
            mode: 0o755,
        };
        let mut deepest = top.clone();
        self.send(0, &mkdir(&deepest));
        for _ in 0..15 {
            deepest = deepest.join(&long).unwrap();
            self.send(0, &mkdir(&deepest));
        }
        // 1 + 15 * 256 + 201 = 4042 bytes.
        let file = deepest.join(&"f".repeat(200)).unwrap();
        self.send(
            0,
            &Request::Create {
                space: space.clone(),
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
            to: to.clone(),
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
            self.send(
                0,
                &Request::Remove {
                    space: space.clone(),
                    path,
                },
            );
        }
    }

    pub fn make(&mut self) {
        // Mostly alice, who can write, so that trees grow deep enough to move.
        let who = if self.rng.chance(40) { 0 } else { self.actor() };
        let space = self.space();
        let path = self.path(&space);
        let request = match self.rng.below(20) {
            0..=9 => Request::Create {
                space,
                path,
                mode: 0o644,
            },
            10..=16 => Request::Mkdir {
                space,
                path,
                mode: 0o755,
            },
            _ => Request::Symlink {
                space,
                path,
                target: "../elsewhere".into(),
            },
        };
        self.send(who, &request);
    }

    pub fn remove(&mut self) {
        let (who, space) = (self.actor(), self.space());
        let path = self.existing_path(&space);
        self.send(who, &Request::Remove { space, path });
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
            to: to.clone(),
        };
        if self.exec(who, &request).is_ok() && from != to {
            self.check_rename(&space, &from, &to, &moved);
        }
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
        let request = match self.rng.below(5) {
            0 => Request::Stat { space, path },
            1 => Request::List { space, path },
            2 => Request::Open { space, path },
            3 => Request::History { space, path },
            _ => Request::Whoami,
        };
        self.send(who, &request);
    }
}

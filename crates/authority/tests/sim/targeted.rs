//! Targeted steps for path limits and late access grants.

use herdfs_core::{
    SpacePath,
    proto::{Perm, Request},
};

use crate::Sim;

impl Sim {
    /// Builds a chain whose deepest path nears the limit, then renames its top.
    pub fn deep_rename(&mut self) {
        let space = self.space();
        let (top, long) = (SpacePath::try_from("z").unwrap(), "d".repeat(255));
        let mut deepest = top.clone();
        let mut mkdir = Request::Mkdir {
            space: space.clone(),
            parent: self.parent(&space, &deepest),
            name: deepest.name().into(),
            mode: 0o755,
        };
        self.send(0, &mkdir);
        for _ in 0..15 {
            deepest = deepest.join(&long).unwrap();
            mkdir = Request::Mkdir {
                space: space.clone(),
                parent: self.parent(&space, &deepest),
                name: deepest.name().into(),
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
                name: file.name().into(),
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
            from_parent: self.parent(&space, &top),
            from_name: top.name().into(),
            to_parent: self.parent(&space, &to),
            to_name: to.name().into(),
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
                    parent,
                    name: path.name().into(),
                },
            );
        }
    }

    /// Checks that versions from before a user's grant stay hidden and unusable (S6, S8).
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
}

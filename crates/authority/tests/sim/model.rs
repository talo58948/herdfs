//! The reference model: plain maps holding what the authority must hold,
//! and the outcome it must give each request.

use std::collections::{BTreeMap, BTreeSet};

use herdfs_authority::Principal;
use herdfs_core::{
    ChunkId, ChunkRef, SpacePath,
    proto::{Entry, EntryId, ErrorCode, Kind, Perm, Request, Version, VersionId},
};

#[derive(Default)]
pub struct Model {
    pub spaces: BTreeMap<String, Space>,
    /// Every version ever acknowledged, including those of removed entries.
    pub versions: BTreeMap<VersionId, Version>,
    /// Every entry id ever handed out.
    pub entry_ids: BTreeSet<EntryId>,
    /// Each chunk a user uploaded, with its length.
    pub uploads: BTreeMap<(String, ChunkId), u32>,
    /// `(user, request id)` of each commit whose entry still exists.
    pub commits: BTreeMap<(String, String), VersionId>,
    /// Acknowledged commits, with their space and entry.
    pub acked: BTreeMap<VersionId, (String, EntryId)>,
    /// Successful guarded commits per `(entry, base)`.
    pub guarded_wins: BTreeMap<(EntryId, Option<VersionId>), usize>,
}

#[derive(Clone, Default)]
pub struct Space {
    pub nodes: BTreeMap<SpacePath, Node>,
    pub acl: BTreeMap<(SpacePath, String), Perm>,
}

#[derive(Clone)]
pub struct Node {
    pub id: EntryId,
    pub kind: Kind,
    pub mode: u32,
    pub mtime_ms: i64,
    pub target: Option<String>,
    /// Version ids, oldest first; the last is the head.
    pub history: Vec<VersionId>,
}

type Outcome = Result<(), ErrorCode>;

fn ensure(holds: bool, code: ErrorCode) -> Outcome {
    if holds { Ok(()) } else { Err(code) }
}

fn is_space_name(name: &str) -> bool {
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c);
    (1..=63).contains(&name.len()) && name.chars().all(allowed) && !name.starts_with(['.', '_', '-'])
}

impl Model {
    pub fn space(&self, space: &str) -> Result<&Space, ErrorCode> {
        self.spaces.get(space).ok_or(ErrorCode::NotFound)
    }

    pub fn perm(&self, space: &str, path: &SpacePath, who: &Principal) -> Option<Perm> {
        let groups: Vec<_> = who.groups.iter().map(|group| format!("group:{group}")).collect();
        let acl = &self.spaces.get(space)?.acl;
        acl.iter()
            .filter(|((at, principal), _)| {
                path.is_within(at) && (*principal == who.user || groups.contains(principal))
            })
            .map(|(_, perm)| *perm)
            .max()
    }

    fn require(&self, who: &Principal, space: &str, path: &SpacePath, need: Perm) -> Outcome {
        self.space(space)?;
        ensure(self.perm(space, path, who) >= Some(need), ErrorCode::Denied)
    }

    pub fn node(&self, space: &str, path: &SpacePath) -> Result<&Node, ErrorCode> {
        self.space(space)?.nodes.get(path).ok_or(ErrorCode::NotFound)
    }

    pub fn node_by_id(&self, space: &str, id: EntryId) -> Option<(&SpacePath, &Node)> {
        self.spaces
            .get(space)?
            .nodes
            .iter()
            .find(|(_, node)| node.id == id)
    }

    pub fn has_children(&self, space: &str, dir: &SpacePath) -> bool {
        self.spaces[space]
            .nodes
            .keys()
            .any(|path| path != dir && path.is_within(dir))
    }

    pub fn entry(&self, space: &str, path: &SpacePath) -> Entry {
        let node = &self.spaces[space].nodes[path];
        let head = node.history.last().copied();
        Entry {
            id: node.id,
            path: path.clone(),
            kind: node.kind,
            mode: node.mode,
            mtime_ms: node.mtime_ms,
            head,
            size: head.map_or(0, |id| self.versions[&id].size),
            target: node.target.clone(),
        }
    }

    pub fn history(&self, node: &Node) -> Vec<Version> {
        node.history.iter().map(|id| self.versions[id].clone()).collect()
    }

    /// The error the authority must answer `request` with, if any.
    pub fn outcome(&self, who: &Principal, request: &Request) -> Outcome {
        use ErrorCode::*;
        match request {
            Request::Whoami => Ok(()),
            Request::CreateSpace { space } => self.new_space(space),
            Request::Fork { space, name } => {
                self.require(who, space, &SpacePath::root(), Perm::Admin)?;
                self.new_space(name)
            }
            Request::Stat { space, path }
            | Request::Open { space, path }
            | Request::History { space, path } => {
                self.require(who, space, path, Perm::Read)?;
                self.node(space, path).map(drop)
            }
            Request::List { space, path } => {
                self.require(who, space, path, Perm::Read)?;
                ensure(self.node(space, path)?.kind == Kind::Dir, NotDir)
            }
            Request::Grant {
                space,
                path,
                version,
                chunks,
            } => {
                self.require(who, space, path, Perm::Read)?;
                ensure(self.node(space, path)?.history.contains(version), NotFound)?;
                let count = self.versions[version].chunks.len();
                ensure(chunks.iter().all(|&index| (index as usize) < count), Invalid)
            }
            Request::Create { space, path, .. }
            | Request::Mkdir { space, path, .. }
            | Request::Symlink { space, path, .. } => {
                self.require(who, space, path, Perm::Write)?;
                ensure(self.node(space, path).is_err(), Exists)?;
                self.parent_dir(space, path)
            }
            Request::Remove { space, path } => {
                self.require(who, space, path, Perm::Write)?;
                ensure(!path.is_root(), Invalid)?;
                let node = self.node(space, path)?;
                ensure(
                    node.kind != Kind::Dir || !self.has_children(space, path),
                    NotEmpty,
                )
            }
            Request::Rename { space, from, to } => self.rename_outcome(who, space, from, to),
            Request::SetAttr { space, path, .. } => {
                self.require(who, space, path, Perm::Write)?;
                self.node(space, path).map(drop)
            }
            Request::Commit {
                space,
                entry,
                request_id,
                base,
                guarded,
                chunks,
            } => self.commit_outcome(who, space, *entry, request_id, *base, *guarded, chunks),
            Request::Restore { space, path, version } => {
                self.require(who, space, path, Perm::Write)?;
                ensure(self.node(space, path)?.history.contains(version), NotFound)
            }
            Request::SetAcl {
                space,
                path,
                principal,
                ..
            } => {
                self.require(who, space, path, Perm::Admin)?;
                let name = principal
                    .strip_prefix("user:")
                    .or_else(|| principal.strip_prefix("group:"));
                ensure(name.is_some_and(|name| !name.is_empty()), Invalid)
            }
        }
    }

    /// Uploads need write on some path of the space.
    pub fn upload_outcome(&self, who: &Principal, space: &str) -> Outcome {
        let acl = &self.space(space)?.acl;
        let mine = |principal: &String| {
            *principal == who.user || who.groups.iter().any(|g| *principal == format!("group:{g}"))
        };
        let writes = acl
            .iter()
            .any(|((_, principal), perm)| mine(principal) && *perm >= Perm::Write);
        ensure(writes, ErrorCode::Denied)
    }

    fn new_space(&self, name: &str) -> Outcome {
        match (is_space_name(name), self.spaces.contains_key(name)) {
            (false, _) => Err(ErrorCode::Invalid),
            (true, true) => Err(ErrorCode::Exists),
            (true, false) => Ok(()),
        }
    }

    fn parent_dir(&self, space: &str, path: &SpacePath) -> Outcome {
        let parent = path.parent().unwrap_or_default();
        match self.node(space, &parent)?.kind {
            Kind::Dir => Ok(()),
            _ => Err(ErrorCode::NotDir),
        }
    }

    fn rename_outcome(&self, who: &Principal, space: &str, from: &SpacePath, to: &SpacePath) -> Outcome {
        self.require(who, space, from, Perm::Write)?;
        self.require(who, space, to, Perm::Write)?;
        let source = self.node(space, from)?;
        if from == to {
            return Ok(());
        }
        ensure(!to.is_within(from), ErrorCode::Invalid)?;
        let Ok(target) = self.node(space, to) else {
            return self.parent_dir(space, to);
        };
        match (source.kind == Kind::Dir, target.kind == Kind::Dir) {
            (true, false) => Err(ErrorCode::NotDir),
            (false, true) => Err(ErrorCode::IsDir),
            (true, true) if self.has_children(space, to) => Err(ErrorCode::NotEmpty),
            _ => Ok(()),
        }
    }

    #[allow(clippy::too_many_arguments, reason = "mirrors the fields of Request::Commit")]
    fn commit_outcome(
        &self,
        who: &Principal,
        space: &str,
        entry: EntryId,
        request_id: &str,
        base: Option<VersionId>,
        guarded: bool,
        chunks: &[ChunkRef],
    ) -> Outcome {
        self.space(space)?;
        let retry = (who.user.clone(), request_id.to_owned());
        if self.commits.contains_key(&retry) {
            return Ok(());
        }
        let (path, node) = self.node_by_id(space, entry).ok_or(ErrorCode::NotFound)?;
        self.require(who, space, path, Perm::Write)?;
        ensure(node.kind == Kind::File, ErrorCode::Invalid)?;
        let base_chunks = match base {
            Some(base) if node.history.contains(&base) => self.versions[&base].chunks.as_slice(),
            Some(_) => return Err(ErrorCode::Invalid),
            None => &[],
        };
        let known = |chunk: &ChunkRef| {
            base_chunks.contains(chunk) || self.uploads.get(&(who.user.clone(), chunk.id)) == Some(&chunk.len)
        };
        ensure(chunks.iter().all(known), ErrorCode::Invalid)?;
        ensure(
            !guarded || node.history.last().copied() == base,
            ErrorCode::Conflict,
        )
    }
}

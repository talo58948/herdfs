//! The reference model: plain maps holding what the authority must hold,
//! and the outcome it must give each request.

use std::collections::{BTreeMap, BTreeSet};

use herdfs_authority::Principal;
use herdfs_core::{
    ChunkId, SpacePath,
    proto::{Entry, EntryId, ErrorCode, Kind, Perm, Request, Version, VersionId},
};

#[derive(Default)]
pub struct Model {
    pub spaces: BTreeMap<String, Space>,
    /// Every version ever acknowledged, including those of removed entries.
    pub versions: BTreeMap<VersionId, Version>,
    /// Every entry id ever handed out.
    pub entry_ids: BTreeSet<EntryId>,
    /// Each chunk a user uploaded: its length, and when.
    pub uploads: BTreeMap<(String, ChunkId), (u32, i64)>,
    /// `(user, request id)` of each commit whose entry still exists: its entry,
    /// version and time. It outlives the version, until collection drops it.
    pub commits: BTreeMap<(String, String), (EntryId, VersionId, i64)>,
    /// Acknowledged commits, with their space and entry.
    pub acked: BTreeMap<VersionId, (String, EntryId)>,
    /// Successful guarded commits per `(entry, base)`.
    pub guarded_wins: BTreeMap<(EntryId, Option<VersionId>), usize>,
}

#[derive(Clone, Default)]
pub struct Space {
    pub nodes: BTreeMap<SpacePath, Node>,
    /// Each row's perm and the time it was first granted.
    pub acl: BTreeMap<(SpacePath, String), (Perm, i64)>,
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

pub type Outcome = Result<(), ErrorCode>;

pub fn ensure(holds: bool, code: ErrorCode) -> Outcome {
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
            Request::Locate { space, entry } => {
                let (path, _) = self.node_by_id(space, *entry).ok_or(NotFound)?;
                self.require(who, space, path, Perm::Read)
            }
            Request::Stat { space, path } if path.is_root() => {
                self.require_somewhere(who, space, Perm::Read)?;
                self.node(space, path).map(drop)
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
                entry,
                version,
                chunks,
            } => {
                ensure(chunks.len() <= 4096, Invalid)?;
                let (path, node) = self.node_by_id(space, *entry).ok_or(NotFound)?;
                self.require(who, space, path, Perm::Read)?;
                ensure(node.history.contains(version), NotFound)?;
                ensure(self.visible(space, path, node, *version, who), NotFound)?;
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
                ..
            } => self.commit_outcome(who, space, *entry, request_id, *base, *guarded, chunks),
            Request::Restore { space, path, version } => {
                self.require(who, space, path, Perm::Write)?;
                let node = self.node(space, path)?;
                ensure(node.history.contains(version), NotFound)?;
                ensure(self.visible(space, path, node, *version, who), NotFound)
            }
            Request::Purge { space, path } => {
                self.require(who, space, path, Perm::Write)?;
                self.node(space, path).map(drop)
            }
            Request::DeleteSpace { space } => self.require(who, space, &SpacePath::root(), Perm::Admin),
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
        let mut moved = self.spaces[space]
            .nodes
            .keys()
            .filter(|path| path.is_within(from));
        ensure(
            moved.all(|path| path.rebase(from, to).is_some()),
            ErrorCode::Invalid,
        )?;
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
}

//! A space `team` created by alice, and short ways to call the authority.
#![allow(dead_code, reason = "each test binary uses a different subset")]

use herdfs_authority::{Authority, Outcome, Principal};
use herdfs_core::{
    ChunkId, ChunkRef, SpacePath,
    proto::{Entry, EntryId, ErrorCode, Perm, Request, Response, Version, VersionId},
};
use tempfile::TempDir;

pub const SPACE: &str = "team";

pub fn user(name: &str, groups: &[&str]) -> Principal {
    Principal {
        user: format!("user:{name}"),
        groups: groups.iter().map(|group| (*group).to_owned()).collect(),
    }
}

pub fn alice() -> Principal {
    user("alice", &[])
}

pub fn bob() -> Principal {
    user("bob", &[])
}

pub fn carol() -> Principal {
    user("carol", &["eng"])
}

pub fn path(text: &str) -> SpacePath {
    SpacePath::try_from(text).unwrap()
}

pub struct Fixture {
    pub authority: Authority,
    pub now_ms: i64,
    dir: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let authority = Authority::open(&dir.path().join("authority.db")).unwrap();
        let mut fixture = Self {
            authority,
            now_ms: 1_000,
            dir,
        };
        fixture
            .call(&alice(), Request::CreateSpace { space: SPACE.into() })
            .unwrap();
        fixture
    }

    /// Drops the authority and opens the same database again.
    pub fn reopen(&mut self) {
        self.authority = Authority::open(&self.dir.path().join("authority.db")).unwrap();
    }

    /// Handles `request` one millisecond after the previous one.
    pub fn outcome(&mut self, who: &Principal, request: Request) -> Result<Outcome, ErrorCode> {
        self.now_ms += 1;
        self.authority
            .handle(who, request, self.now_ms)
            .map_err(|error| error.code)
    }

    pub fn call(&mut self, who: &Principal, request: Request) -> Result<Response, ErrorCode> {
        match self.outcome(who, request)? {
            Outcome::Reply(reply) => Ok(reply),
            Outcome::Sign { .. } => panic!("only Grant signs"),
        }
    }

    pub fn entry(&mut self, who: &Principal, request: Request) -> Result<Entry, ErrorCode> {
        match self.call(who, request)? {
            Response::Entry { entry } => Ok(entry),
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    pub fn stat(&mut self, who: &Principal, at: &str) -> Result<Entry, ErrorCode> {
        self.entry(
            who,
            Request::Stat {
                space: SPACE.into(),
                path: path(at),
            },
        )
    }

    pub fn mkdir(&mut self, who: &Principal, at: &str) -> Result<Entry, ErrorCode> {
        self.entry(
            who,
            Request::Mkdir {
                space: SPACE.into(),
                path: path(at),
                mode: 0o755,
            },
        )
    }

    pub fn create(&mut self, who: &Principal, at: &str) -> Result<Entry, ErrorCode> {
        self.entry(
            who,
            Request::Create {
                space: SPACE.into(),
                path: path(at),
                mode: 0o644,
            },
        )
    }

    pub fn remove(&mut self, who: &Principal, at: &str) -> Result<Response, ErrorCode> {
        self.call(
            who,
            Request::Remove {
                space: SPACE.into(),
                path: path(at),
            },
        )
    }

    pub fn rename(&mut self, who: &Principal, from: &str, to: &str) -> Result<Response, ErrorCode> {
        self.call(
            who,
            Request::Rename {
                space: SPACE.into(),
                from: path(from),
                to: path(to),
            },
        )
    }

    pub fn list(&mut self, who: &Principal, at: &str) -> Result<Vec<String>, ErrorCode> {
        match self.call(
            who,
            Request::List {
                space: SPACE.into(),
                path: path(at),
            },
        )? {
            Response::Entries { entries } => Ok(entries.into_iter().map(|entry| entry.path.into()).collect()),
            other => panic!("expected entries, got {other:?}"),
        }
    }

    pub fn set_acl(
        &mut self,
        who: &Principal,
        at: &str,
        principal: &str,
        perm: Option<Perm>,
    ) -> Result<(), ErrorCode> {
        let request = Request::SetAcl {
            space: SPACE.into(),
            path: path(at),
            principal: principal.into(),
            perm,
        };
        self.call(who, request).map(drop)
    }

    /// Records `bytes` as uploaded by `who` and returns its reference.
    pub fn upload(&mut self, who: &Principal, bytes: &[u8]) -> Result<ChunkRef, ErrorCode> {
        let id = ChunkId::of(bytes);
        self.now_ms += 1;
        self.authority
            .record_upload(who, SPACE, id, self.now_ms)
            .map_err(|error| error.code)?;
        Ok(ChunkRef {
            id,
            len: u32::try_from(bytes.len()).unwrap(),
        })
    }

    pub fn commit(
        &mut self,
        who: &Principal,
        entry: EntryId,
        request_id: &str,
        base: Option<VersionId>,
        guarded: bool,
        chunks: &[ChunkRef],
    ) -> Result<Version, ErrorCode> {
        let request = Request::Commit {
            space: SPACE.into(),
            entry,
            request_id: request_id.into(),
            base,
            guarded,
            chunks: chunks.to_vec(),
        };
        match self.call(who, request)? {
            Response::Version { version } => Ok(version),
            other => panic!("expected a version, got {other:?}"),
        }
    }

    /// Creates `at` as alice and commits one uploaded chunk of `bytes` to it.
    pub fn file(&mut self, at: &str, bytes: &[u8]) -> (Entry, Version) {
        let entry = self.create(&alice(), at).unwrap();
        let chunk = self.upload(&alice(), bytes).unwrap();
        let version = self
            .commit(&alice(), entry.id, &format!("{at}-1"), None, false, &[chunk])
            .unwrap();
        (entry, version)
    }

    pub fn history(&mut self, who: &Principal, at: &str) -> Result<Vec<Version>, ErrorCode> {
        match self.call(
            who,
            Request::History {
                space: SPACE.into(),
                path: path(at),
            },
        )? {
            Response::History { versions } => Ok(versions),
            other => panic!("expected history, got {other:?}"),
        }
    }

    pub fn grant(
        &mut self,
        who: &Principal,
        at: &str,
        version: VersionId,
        chunks: &[u32],
    ) -> Result<Vec<ChunkId>, ErrorCode> {
        let request = Request::Grant {
            space: SPACE.into(),
            path: path(at),
            version,
            chunks: chunks.to_vec(),
        };
        match self.outcome(who, request)? {
            Outcome::Sign { chunks, expires_ms } => {
                assert_eq!(
                    expires_ms,
                    self.now_ms + 5 * 60 * 1000,
                    "grants live five minutes"
                );
                Ok(chunks)
            }
            Outcome::Reply(reply) => panic!("expected grants, got {reply:?}"),
        }
    }
}

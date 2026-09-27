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
    let groups = groups.iter().map(|group| (*group).to_owned()).collect();
    Principal {
        user: format!("user:{name}"),
        groups,
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
    now_ms: i64,
    _dir: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let authority = Authority::open(&dir.path().join("authority.db")).unwrap();
        let mut fixture = Self {
            authority,
            now_ms: 1_000,
            _dir: dir,
        };
        fixture
            .call(&alice(), Request::CreateSpace { space: SPACE.into() })
            .unwrap();
        fixture
    }

    /// Handles `request` one millisecond after the previous one.
    pub fn call(&mut self, who: &Principal, request: Request) -> Result<Response, ErrorCode> {
        self.now_ms += 1;
        match self.authority.handle(who, request, self.now_ms) {
            Ok(Outcome::Reply(reply)) => Ok(reply),
            Ok(Outcome::Sign { .. }) => panic!("only Grant signs"),
            Err(error) => Err(error.code),
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

    pub fn set_acl(&mut self, at: &str, principal: &str, perm: Option<Perm>) {
        let principal = principal.into();
        let request = Request::SetAcl {
            space: SPACE.into(),
            path: path(at),
            principal,
            perm,
        };
        self.call(&alice(), request).unwrap();
    }

    /// Uploads `bytes` as `who`, as the server records it.
    pub fn upload(&mut self, who: &Principal, bytes: &[u8]) -> Result<ChunkRef, ErrorCode> {
        let chunk = ChunkRef {
            id: ChunkId::of(bytes),
            len: u32::try_from(bytes.len()).unwrap(),
        };
        self.now_ms += 1;
        let recorded = self.authority.record_upload(who, SPACE, chunk, self.now_ms);
        recorded.map_err(|error| error.code)?;
        Ok(chunk)
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
        let (request_id, chunks) = (request_id.into(), chunks.to_vec());
        let request = Request::Commit {
            space: SPACE.into(),
            entry,
            request_id,
            base,
            guarded,
            chunks,
        };
        match self.call(who, request)? {
            Response::Version { version } => Ok(version),
            other => panic!("expected a version, got {other:?}"),
        }
    }

    /// Creates `at` as alice and commits `bytes` to it as one chunk.
    pub fn file(&mut self, at: &str, bytes: &[u8]) -> (Entry, Version) {
        let entry = self.create(&alice(), at).unwrap();
        let chunk = self.upload(&alice(), bytes).unwrap();
        let version = self
            .commit(&alice(), entry.id, at, None, false, &[chunk])
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
        self.now_ms += 1;
        let request = Request::Grant {
            space: SPACE.into(),
            path: path(at),
            version,
            chunks: chunks.to_vec(),
        };
        match self.authority.handle(who, request, self.now_ms) {
            Ok(Outcome::Sign { chunks, expires_ms }) => {
                assert_eq!(
                    expires_ms,
                    self.now_ms + 5 * 60 * 1000,
                    "grants live five minutes"
                );
                Ok(chunks)
            }
            Ok(Outcome::Reply(reply)) => panic!("expected grants, got {reply:?}"),
            Err(error) => Err(error.code),
        }
    }

    fn entry(&mut self, who: &Principal, request: Request) -> Result<Entry, ErrorCode> {
        match self.call(who, request)? {
            Response::Entry { entry } => Ok(entry),
            other => panic!("expected an entry, got {other:?}"),
        }
    }
}

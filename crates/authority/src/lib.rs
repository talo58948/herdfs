//! The herdfs authority: the namespace, versions, ACLs and grants of every
//! space, kept in SQLite. [`Authority`] is a synchronous state machine;
//! [`server`] puts it behind HTTP.

mod acl;
mod db;
mod namespace;
pub mod server;
mod spaces;
mod versions;

use std::path::Path;

use herdfs_core::{
    ChunkId, ChunkRef,
    proto::{self, ErrorCode, Kind, Request, Response},
};
use rusqlite::Connection;

/// How long a presigned chunk URL stays valid.
const GRANT_TTL_MS: i64 = 5 * 60 * 1000;
/// The clock of calls that read none: upload bookkeeping.
const NO_CLOCK: i64 = 0;

/// An authenticated caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// `user:<sub>`.
    pub user: String,
    /// Group names, without the `group:` prefix.
    pub groups: Vec<String>,
}

/// What the server sends back for a handled request.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Reply(Response),
    /// The answer to `Grant`: presign these chunks, valid until `expires_ms`.
    Sign {
        chunks: Vec<ChunkId>,
        expires_ms: i64,
    },
}

pub struct Authority {
    db: Connection,
}

impl Authority {
    /// Opens the database at `db`, creating the schema if it is missing.
    pub fn open(db: &Path) -> anyhow::Result<Self> {
        let db = Connection::open(db)?;
        db.execute_batch(db::SCHEMA)?;
        db.set_prepared_statement_cache_capacity(64);
        Ok(Self { db })
    }

    /// Checks and applies one request in its own transaction.
    pub fn handle(
        &mut self,
        who: &Principal,
        request: Request,
        now_ms: i64,
    ) -> Result<Outcome, proto::Error> {
        self.transact(who, now_ms, |call| call.dispatch(request))
    }

    /// Fails unless `who` may upload to `space`, which takes write on some
    /// path there. The server asks before it stores any bytes.
    pub fn may_upload(&mut self, who: &Principal, space: &str) -> Result<(), proto::Error> {
        self.transact(who, NO_CLOCK, |call| call.may_upload(space))
    }

    /// Records that `who` uploaded `chunk`, so their commits may reference it
    /// at that length.
    pub fn record_upload(
        &mut self,
        who: &Principal,
        space: &str,
        chunk: ChunkRef,
    ) -> Result<(), proto::Error> {
        self.transact(who, NO_CLOCK, |call| call.record_upload(space, chunk))
    }

    fn transact<T>(
        &mut self,
        who: &Principal,
        now_ms: i64,
        run: impl FnOnce(&Call) -> Result<T>,
    ) -> Result<T, proto::Error> {
        let tx = self.db.transaction().map_err(Fail::from)?;
        let out = run(&Call { db: &tx, who, now_ms })?;
        tx.commit().map_err(Fail::from)?;
        Ok(out)
    }
}

/// One request being handled: its open transaction, its caller and its clock.
struct Call<'a> {
    db: &'a Connection,
    who: &'a Principal,
    now_ms: i64,
}

impl Call<'_> {
    fn dispatch(&self, request: Request) -> Result<Outcome> {
        let reply = match request {
            Request::Whoami => Response::Me {
                principal: self.who.user.clone(),
                groups: self.who.groups.clone(),
            },
            Request::CreateSpace { space } => self.create_space(&space)?,
            Request::Fork { space, name } => self.fork(&space, &name)?,
            Request::Stat { space, path } => self.stat(&space, &path)?,
            Request::List { space, path } => self.list(&space, &path)?,
            Request::Open { space, path } => self.open(&space, &path)?,
            Request::Grant {
                space,
                path,
                version,
                chunks,
            } => {
                return self.grant(&space, &path, version, &chunks);
            }
            Request::Create { space, path, mode } => self.make(&space, &path, Kind::File, mode, None)?,
            Request::Mkdir { space, path, mode } => self.make(&space, &path, Kind::Dir, mode, None)?,
            Request::Symlink { space, path, target } => self.make(
                &space,
                &path,
                Kind::Symlink,
                namespace::SYMLINK_MODE,
                Some(&target),
            )?,
            Request::Remove { space, path } => self.remove(&space, &path)?,
            Request::Rename { space, from, to } => self.rename(&space, &from, &to)?,
            Request::SetAttr {
                space,
                path,
                mode,
                mtime_ms,
            } => self.set_attr(&space, &path, mode, mtime_ms)?,
            Request::Commit {
                space,
                entry,
                request_id,
                base,
                guarded,
                chunks,
            } => self.commit(&space, entry, &request_id, base, guarded, &chunks)?,
            Request::History { space, path } => self.history(&space, &path)?,
            Request::Restore { space, path, version } => self.restore(&space, &path, version)?,
            Request::SetAcl {
                space,
                path,
                principal,
                perm,
            } => self.set_acl(&space, &path, &principal, perm)?,
        };
        Ok(Outcome::Reply(reply))
    }
}

/// Why a request stopped: refused under the protocol, or failed in the database.
#[derive(Debug)]
enum Fail {
    Refused(proto::Error),
    Db(rusqlite::Error),
}

type Result<T, E = Fail> = std::result::Result<T, E>;

fn refuse(code: ErrorCode, message: impl Into<String>) -> Fail {
    Fail::Refused(proto::Error::new(code, message))
}

impl From<rusqlite::Error> for Fail {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(error)
    }
}

impl From<Fail> for proto::Error {
    fn from(fail: Fail) -> Self {
        match fail {
            Fail::Refused(error) => error,
            Fail::Db(error) => {
                tracing::error!(%error, "database failure");
                Self::new(ErrorCode::Unavailable, "database failure")
            }
        }
    }
}

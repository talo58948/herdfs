//! NFSv3 translation for the loopback macOS mount.

mod adapter;
mod flow;
mod identity;
mod namespace;

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use nfsserve::nfs::{self, nfsstat3};
use herdfs_core::proto::EntryId;
use tokio::sync::Mutex as AsyncMutex;

use super::{Attributes, FileKind, Space};
use crate::{Error, lock};

pub(crate) use identity::Identity;

const MAGIC: &[u8; 4] = b"OSNF";
const FORMAT: u32 = 1;
const FSID: u64 = 0x4f53_4e46;
const MAX_FILE_SIZE: u64 = 350 * 1024 * 1024 * 1024;

pub(crate) struct Nfs {
    space: Arc<Space>,
    identity: Arc<Identity>,
    handle_prefix: [u8; 24],
    verifier: [u8; 8],
    flows: Arc<Mutex<HashMap<EntryId, Arc<AsyncMutex<flow::Flow>>>>>,
}

impl Nfs {
    pub(crate) fn new(space: Arc<Space>, identity: Arc<Identity>) -> Self {
        let mut handle_prefix = [0; 24];
        handle_prefix[..4].copy_from_slice(MAGIC);
        handle_prefix[4..8].copy_from_slice(&FORMAT.to_be_bytes());
        handle_prefix[8..].copy_from_slice(&identity.export_id());
        Self {
            space,
            identity,
            handle_prefix,
            verifier: rand::random(),
            flows: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn flow(&self, entry: EntryId) -> Arc<AsyncMutex<flow::Flow>> {
        lock(&self.flows)
            .entry(entry)
            .or_insert_with(|| Arc::new(AsyncMutex::new(flow::Flow::default())))
            .clone()
    }

    fn existing_flow(&self, entry: EntryId) -> Option<Arc<AsyncMutex<flow::Flow>>> {
        lock(&self.flows).get(&entry).cloned()
    }

    fn ino(&self, id: u64) -> super::InodeNo {
        self.space.ino(EntryId(id))
    }

    fn name(name: &nfs::filename3) -> Result<OsString, nfsstat3> {
        let name = std::str::from_utf8(name.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        Ok(OsString::from(name))
    }

    fn attr(&self, attr: &Attributes) -> nfs::fattr3 {
        let fileid = self.space.entry_id(attr.ino).0;
        let ftype = match attr.kind {
            FileKind::File => nfs::ftype3::NF3REG,
            FileKind::Directory => nfs::ftype3::NF3DIR,
            FileKind::Symlink => nfs::ftype3::NF3LNK,
        };
        let time = nfs_time(attr.time);
        nfs::fattr3 {
            ftype,
            mode: u32::from(attr.perm),
            nlink: attr.nlink,
            uid: attr.uid,
            gid: attr.gid,
            size: attr.size,
            used: attr.blocks.saturating_mul(512),
            rdev: nfs::specdata3::default(),
            fsid: FSID,
            fileid,
            atime: time,
            mtime: time,
            ctime: time,
        }
    }

    fn stale(error: Error) -> nfsstat3 {
        if error.errno() == libc::ENOENT {
            nfsstat3::NFS3ERR_STALE
        } else {
            error.into()
        }
    }
}

impl From<Error> for nfsstat3 {
    fn from(error: Error) -> Self {
        match error.errno() {
            libc::EPERM => nfsstat3::NFS3ERR_PERM,
            libc::ENOENT => nfsstat3::NFS3ERR_NOENT,
            libc::EACCES => nfsstat3::NFS3ERR_ACCES,
            libc::EEXIST => nfsstat3::NFS3ERR_EXIST,
            libc::ENOTDIR => nfsstat3::NFS3ERR_NOTDIR,
            libc::EISDIR => nfsstat3::NFS3ERR_ISDIR,
            libc::EINVAL => nfsstat3::NFS3ERR_INVAL,
            libc::EFBIG => nfsstat3::NFS3ERR_FBIG,
            libc::ENOSPC => nfsstat3::NFS3ERR_NOSPC,
            libc::EROFS => nfsstat3::NFS3ERR_ROFS,
            libc::ENAMETOOLONG => nfsstat3::NFS3ERR_NAMETOOLONG,
            libc::ENOTEMPTY => nfsstat3::NFS3ERR_NOTEMPTY,
            libc::ESTALE => nfsstat3::NFS3ERR_STALE,
            libc::ENOTSUP => nfsstat3::NFS3ERR_NOTSUPP,
            _ => nfsstat3::NFS3ERR_IO,
        }
    }
}

fn nfs_time(time: SystemTime) -> nfs::nfstime3 {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    nfs::nfstime3 {
        seconds: u32::try_from(elapsed.as_secs()).unwrap_or(u32::MAX),
        nseconds: elapsed.subsec_nanos(),
    }
}

fn nfs_unix_ms(time: nfs::nfstime3) -> i64 {
    i64::from(time.seconds) * 1_000 + i64::from(time.nseconds / 1_000_000)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use nfsserve::vfs::NFSFileSystem;

    use super::*;
    use crate::cache::Cache;
    use crate::fs::{Inodes, Kernel};
    use crate::journal::Journal;
    use crate::remote::Remote;
    use crate::stats::Stats;
    use crate::strategy::{AccessPlan, Decider};

    fn adapter(root: &std::path::Path) -> (Nfs, Arc<Identity>) {
        let journal_dir = root.join("journal");
        let inodes = Arc::new(Inodes::default());
        let (journal, _) = Journal::open(journal_dir.clone(), &inodes).unwrap();
        let token = root.join("token");
        fs::write(&token, "test").unwrap();
        let remote = Remote::new("http://127.0.0.1:9", "test", token, &[]).unwrap();
        let cache = Cache::open(root.join("cache"), 1 << 20, &[]).unwrap();
        let entry = EntryId(7);
        let kernel = Arc::new(Kernel::new(entry));
        let pins = root.join("pins.json");
        let kept = Arc::new(crate::kept::Kept::new(
            pins.clone(),
            crate::kept::load(&pins).unwrap(),
        ));
        let (commits, _) = tokio::sync::watch::channel(false);
        let space = Arc::new(Space::new(
            Arc::new(remote),
            Arc::new(cache),
            Arc::new(journal),
            commits,
            kept,
            Arc::new(Stats::default()),
            Decider::Fixed(AccessPlan::STREAM),
            kernel,
            inodes,
        ));
        let identity = Arc::new(Identity::open(&journal_dir).unwrap());
        (Nfs::new(space, identity.clone()), identity)
    }

    // contract: M1
    #[test]
    fn handles_and_export_identity_survive_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let (first, identity) = adapter(root.path());
        identity.set_port(23_456).unwrap();
        let handle = first.id_to_fh(42);
        assert_eq!(handle.data.len(), 32);
        assert!(matches!(first.fh_to_id(&handle), Ok(42)));
        drop(first);

        let (restarted, identity) = adapter(root.path());
        assert_eq!(identity.port(), 23_456);
        assert_eq!(restarted.id_to_fh(42).data, handle.data);
        assert!(matches!(restarted.fh_to_id(&handle), Ok(42)));
    }

    // contract: C4a, M1
    #[test]
    fn write_verifier_changes_for_each_server_incarnation() {
        let root = tempfile::tempdir().unwrap();
        let first = adapter(root.path()).0.write_verifier();
        let second = adapter(root.path()).0.write_verifier();
        assert_ne!(first, second);
    }

    // contract: M1
    #[test]
    fn exclusive_create_verifier_is_durable() {
        let root = tempfile::tempdir().unwrap();
        let journal = root.path().join("journal");
        fs::create_dir_all(&journal).unwrap();
        let identity = Identity::open(&journal).unwrap();
        identity
            .remember(EntryId(1), "file".into(), [9; 8], EntryId(2))
            .unwrap();
        drop(identity);
        let identity = Identity::open(&journal).unwrap();
        assert_eq!(identity.replay(EntryId(1), "file", [9; 8]), Some(EntryId(2)));
    }
}

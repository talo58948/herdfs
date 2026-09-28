use std::os::unix::fs::FileExt;

use herdfs_core::{ChunkId, ChunkRef};

use super::commit::Outcome;
use super::*;
use crate::cache::Cache;
use crate::fs::Kernel;
use crate::remote::Remote;
use herdfs_core::SpacePath;
use herdfs_core::proto::{self, Entry, Kind, VersionId};
use std::time::Instant;

const ENTRY: EntryId = EntryId(7);

fn base() -> Version {
    let chunks = vec![ChunkRef {
        id: ChunkId::of(b"old"),
        len: 3,
    }];
    Version {
        id: VersionId(3),
        entry: ENTRY,
        size: 3,
        author: "user:a".into(),
        created_ms: 0,
        chunks,
    }
}

/// A journal with the inodes that hold its entries' changes and a committer, as a mount has
/// them, driven for `ENTRY` the way handles and the committer drive it.
struct Mount {
    journal: Arc<Journal>,
    inodes: Arc<Inodes>,
    committer: Committer,
    _other: tempfile::TempDir,
}

impl Mount {
    fn open(dir: &Path) -> (Self, mpsc::UnboundedReceiver<EntryId>) {
        let inodes = Arc::new(Inodes::default());
        let (journal, woken) = Journal::open(dir.into(), &inodes).unwrap();
        let (journal, other) = (Arc::new(journal), tempfile::tempdir().unwrap());
        let token = other.path().join("token");
        fs::write(&token, "t").unwrap();
        let remote = Remote::new("http://127.0.0.1:9", "test", token, &[]).unwrap();
        let cache = Cache::open(other.path().join("cache"), 1 << 20).unwrap();
        let committer = Committer {
            journal: journal.clone(),
            inodes: inodes.clone(),
            remote: Arc::new(remote),
            cache: Arc::new(cache),
            stats: Arc::default(),
            kernel: Arc::new(Kernel::new(EntryId(1))),
        };
        let mount = Self {
            journal,
            inodes,
            committer,
            _other: other,
        };
        (mount, woken)
    }

    fn local<T>(&self, change: impl FnOnce(&mut Local, &Journal) -> T) -> T {
        let changed = self
            .inodes
            .local(ENTRY, false, |local| change(local, &self.journal));
        changed.unwrap()
    }

    fn opened(&self) {
        self.inodes.local(ENTRY, true, Local::opened);
    }

    fn closed(&self) {
        self.inodes.local(ENTRY, false, Local::closed);
    }

    fn install(&self, base: Option<&Version>) {
        let scratch = self.journal.scratch(ENTRY).unwrap();
        let install = |local: &mut Local| local.install(&self.journal, ENTRY, scratch, base);
        self.inodes.local(ENTRY, true, install).unwrap().unwrap();
    }

    fn write(&self, off: u64, bytes: &[u8]) {
        self.local(|local, journal| local.write(journal, ENTRY, off, bytes))
            .unwrap();
    }

    fn truncate(&self) {
        self.local(|local, journal| local.set_len(journal, ENTRY, 0))
            .unwrap();
    }

    fn seal(&self) {
        self.local(|local, journal| local.seal(journal, ENTRY)).unwrap();
    }

    fn changes(&self) -> Option<Arc<File>> {
        self.inodes.local(ENTRY, false, |local| local.copy()).flatten()
    }

    fn set_mtime(&self, mtime_ms: i64) -> bool {
        let set = |local: &mut Local| local.set_mtime(&self.journal, ENTRY, mtime_ms);
        self.inodes.local(ENTRY, false, set).unwrap_or(Ok(false)).unwrap()
    }

    fn pending_count(&self) -> usize {
        self.inodes.count(Local::is_pending)
    }
}

/// A mount with `bytes` written to a sealed copy of `ENTRY`, whose handle is closed.
fn sealed(dir: &Path, bytes: &[u8]) -> (Mount, mpsc::UnboundedReceiver<EntryId>) {
    let (mount, woken) = Mount::open(dir);
    mount.opened();
    mount.install(Some(&base()));
    mount.write(0, bytes);
    mount.seal();
    mount.closed();
    (mount, woken)
}

/// What this client reads for `ENTRY` while its journal holds changes.
fn read(mount: &Mount) -> Vec<u8> {
    let copy = mount.changes().unwrap();
    let mut bytes = vec![0; copy.metadata().unwrap().len() as usize];
    copy.read_exact_at(&mut bytes, 0).unwrap();
    bytes
}

// contract: C4a
#[test]
fn a_sealed_write_is_recovered_after_a_restart_and_an_unclosed_one_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, mut woken) = sealed(dir.path(), b"closed");
    assert_eq!(woken.try_recv().unwrap(), ENTRY);
    let before = mount.committer.take(ENTRY).unwrap().record;
    assert_eq!(before.base, Some(base()));
    mount.opened();
    mount.truncate();
    mount.write(0, b"never closed");
    drop(mount);

    let (mount, mut woken) = Mount::open(dir.path());
    assert_eq!(woken.try_recv().unwrap(), ENTRY);
    let job = mount.committer.take(ENTRY).unwrap();
    assert_eq!(job.record, before);
    assert_eq!(fs::read(job.sealed).unwrap(), b"closed");
    assert_eq!(read(&mount), b"closed");
    assert!(!dir.path().join("7.work").exists());
}

// contract: C3
#[test]
fn a_sealed_copy_never_changes_under_the_committer() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = sealed(dir.path(), b"first");
    let first = mount.committer.take(ENTRY).unwrap();
    mount.opened();
    mount.write(5, b" and more");
    assert_eq!(read(&mount), b"first and more");
    mount.truncate();
    mount.write(0, b"second");
    mount.seal();
    mount.closed();
    assert_eq!(fs::read(&first.sealed).unwrap(), b"first");

    mount.committer.finish(&first, Outcome::Committed(base()));
    assert!(!first.sealed.exists(), "a newer seal replaced it");
    let second = mount.committer.take(ENTRY).unwrap();
    assert_eq!(fs::read(&second.sealed).unwrap(), b"second");
    assert_eq!(read(&mount), b"second");
}

// contract: C1, C2, C4a
#[test]
fn changes_shadow_reads_until_committed_then_the_commit_does() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = sealed(dir.path(), b"new");
    mount.opened();
    assert_eq!(mount.pending_count(), 1);

    let job = mount.committer.take(ENTRY).unwrap();
    mount.committer.finish(&job, Outcome::Retrying);
    assert_eq!(read(&mount), b"new");
    let version = Version {
        id: VersionId(4),
        ..base()
    };
    mount.committer.finish(&job, Outcome::Committed(version));
    assert!(mount.changes().is_none(), "nothing left to shadow the authority");
    assert!(mount.committer.take(ENTRY).is_none());
    mount.closed();
    assert_eq!(mount.pending_count(), 0);
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        1,
        "only rejected/ is left"
    );
}

// contract: C12
#[test]
fn a_rejected_write_is_set_aside_with_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = sealed(dir.path(), b"denied");
    let job = mount.committer.take(ENTRY).unwrap();
    mount.committer.finish(&job, Outcome::Rejected);
    assert!(mount.committer.take(ENTRY).is_none());

    let kept = dir
        .path()
        .join(REJECTED)
        .join(format!("7-{}", job.record.request_id));
    assert_eq!(fs::read(kept.with_extension("copy")).unwrap(), b"denied");
    let record: Record = serde_json::from_slice(&fs::read(kept.with_extension("json")).unwrap()).unwrap();
    assert_eq!(record, job.record);
    assert!(!dir.path().join("7.json").exists());
}

// contract: none (docs/architecture.md: mtime is stored and reported)
#[test]
fn a_set_mtime_rides_the_next_commit_unless_one_is_under_way() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = Mount::open(dir.path());
    assert!(!mount.set_mtime(1), "no copy: set at the authority");
    mount.opened();
    mount.install(None);
    mount.write(0, b"copied");
    assert!(mount.set_mtime(2));
    mount.seal();
    mount.closed();
    assert!(mount.set_mtime(3), "pending: the record takes it");
    let job = mount.committer.take(ENTRY).unwrap();
    assert_eq!(job.record.mtime_ms, Some(3));
    assert!(!mount.set_mtime(4), "under way: set after it lands");
}

// contract: C4a
#[test]
fn a_record_whose_commit_settled_before_a_crash_is_dropped_at_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = sealed(dir.path(), b"committed");
    let job = mount.committer.take(ENTRY).unwrap();
    drop(mount);
    fs::remove_file(&job.sealed).unwrap();
    let (mount, _woken) = Mount::open(dir.path());
    assert!(mount.committer.take(ENTRY).is_none());
    assert!(!dir.path().join("7.json").exists());
}

// contract: none (docs/architecture.md: a file becomes visible to other hosts on close)
#[test]
fn a_file_closed_again_and_again_commits_within_a_second_of_its_first_close() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = sealed(dir.path(), b"first");
    mount.opened();
    std::thread::sleep(Duration::from_millis(900));
    mount.write(0, b"again");
    mount.seal();
    let quiet = mount.local(|local, _| local.quiet());
    assert!(quiet.is_none_or(|left| left <= Duration::from_millis(100)));
}

// contract: C10
#[test]
fn a_removed_entrys_changes_are_dropped_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = sealed(dir.path(), b"scratch");
    mount.local(|local, journal| local.discard(journal, ENTRY));
    assert!(mount.changes().is_none() && mount.committer.take(ENTRY).is_none());
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        1,
        "only rejected/ is left"
    );
}

// contract: none (docs/architecture.md: mtime is stored and reported)
#[test]
fn a_commit_keeps_the_time_of_the_last_write() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = sealed(dir.path(), b"written");
    let job = mount.committer.take(ENTRY).unwrap();
    let written = fs::metadata(&job.sealed).unwrap().modified().unwrap();
    assert_eq!(job.record.mtime_ms, Some(crate::unix_ms(written)));
}

// contract: C1
#[test]
fn remote_state_from_before_a_settled_commit_is_never_paired_with_the_journal_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = sealed(dir.path(), b"appended");
    let inodes = &mount.inodes;
    let entry = Entry {
        id: ENTRY,
        path: SpacePath::try_from("f").unwrap(),
        kind: Kind::File,
        mode: 0o644,
        mtime_ms: 0,
        head: Some(VersionId(3)),
        size: 3,
        target: None,
    };
    let before = Instant::now();
    inodes.observe(&entry, before, false);
    let view = || inodes.with(ENTRY, |state| state.view()).flatten().unwrap();
    let job = mount.committer.take(ENTRY).unwrap();
    assert!(view().1.is_some(), "while the commit runs, its copy shows");
    let committed = Version {
        id: VersionId(4),
        size: 8,
        ..base()
    };
    mount.committer.finish(&job, Outcome::Committed(committed));
    inodes.observe(&entry, before, false);
    let (settled, local) = view();
    assert_eq!(
        (settled.head, settled.size),
        (Some(VersionId(4)), 8),
        "state asked for before the settle is stale once its copy is gone"
    );
    assert!(local.is_none());
}

/// Sends `record` through a fake authority that refuses its first `conflicts` commits with
/// `Conflict`, as one that no longer retains the base does; returns the bases it was sent.
async fn sent_to_retention(
    record: &Record,
    conflicts: usize,
) -> (Vec<Option<VersionId>>, crate::Result<Version>) {
    let bases = std::sync::Mutex::new(Vec::new());
    let attempt = |record: Record| {
        let mut bases = bases.lock().unwrap();
        bases.push(record.base.map(|base| base.id));
        let refused = proto::Error {
            code: proto::ErrorCode::Conflict,
            message: "base not retained".into(),
        };
        let answer = if bases.len() <= conflicts {
            Err(refused.into())
        } else {
            Ok(base())
        };
        std::future::ready(answer)
    };
    let sent = super::commit::anew(record, attempt).await;
    (bases.into_inner().unwrap(), sent)
}

// contract: C4a, C14
#[tokio::test]
async fn a_commit_whose_base_is_no_longer_retained_is_sent_once_more_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = sealed(dir.path(), b"new");
    let record = mount.committer.take(ENTRY).unwrap().record;
    let (bases, sent) = sent_to_retention(&record, 1).await;
    assert_eq!(bases, [Some(VersionId(3)), None]);
    assert!(sent.is_ok(), "committed without its base");
    let (bases, sent) = sent_to_retention(&record, 2).await;
    assert_eq!(bases, [Some(VersionId(3)), None], "sent twice, never more");
    assert!(
        sent.is_err_and(|err| err.is_final()),
        "a second Conflict is final"
    );
}

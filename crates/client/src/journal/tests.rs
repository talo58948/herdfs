use std::os::unix::fs::FileExt;

use herdfs_core::{ChunkId, ChunkRef};

use super::commit::Outcome;
use super::*;
use crate::cache::Cache;
use crate::fs::{Kernel, Space};
use crate::remote::Remote;
use crate::stats::Stats;
use crate::strategy::{Decider, Strategy};
use herdfs_core::proto::VersionId;

mod commits;

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
        let (remote, cache, stats, kernel) = (
            Arc::new(remote),
            Arc::new(cache),
            Arc::new(Stats::default()),
            Arc::new(Kernel::new(EntryId(1))),
        );
        let space = Space::new(
            remote,
            cache,
            journal.clone(),
            stats,
            Decider::Fixed(Strategy::Stream),
            kernel,
            inodes.clone(),
        )
        .unwrap();
        let committer = Committer {
            space: Arc::new(space),
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

    mount.committer.finish(&first, Outcome::Committed(Some(base())));
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
    mount.committer.finish(&job, Outcome::Committed(Some(version)));
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

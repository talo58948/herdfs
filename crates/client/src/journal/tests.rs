use std::os::unix::fs::FileExt;

use herdfs_core::{ChunkId, ChunkRef};

use super::commit::Outcome;
use super::*;
use herdfs_core::proto::VersionId;

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

/// A journal with `bytes` written to a sealed copy of `ENTRY`, whose handle is closed.
fn sealed(dir: &Path, bytes: &[u8]) -> (Arc<Journal>, mpsc::UnboundedReceiver<EntryId>) {
    let (journal, woken) = Journal::open(dir.into()).unwrap();
    let journal = Arc::new(journal);
    let registration = journal.opened(ENTRY);
    journal
        .install(ENTRY, journal.scratch(ENTRY).unwrap(), Some(&base()))
        .unwrap();
    journal.write(ENTRY, 0, bytes).unwrap();
    journal.seal(ENTRY).unwrap();
    drop(registration);
    (journal, woken)
}

/// What this client reads for `ENTRY` while its journal holds changes.
fn read(journal: &Journal) -> Vec<u8> {
    let copy = journal.changes(ENTRY).unwrap();
    let mut bytes = vec![0; copy.metadata().unwrap().len() as usize];
    copy.read_exact_at(&mut bytes, 0).unwrap();
    bytes
}

// contract: C4a
#[test]
fn a_sealed_write_is_recovered_after_a_restart_and_an_unclosed_one_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, mut woken) = sealed(dir.path(), b"closed");
    assert_eq!(woken.try_recv().unwrap(), ENTRY);
    let before = journal.next(ENTRY).unwrap().record;
    assert_eq!(before.base, Some(base()));
    let _writer = journal.opened(ENTRY);
    journal.attach(ENTRY, true).unwrap();
    journal.write(ENTRY, 0, b"never closed").unwrap();
    drop(journal);

    let (journal, mut woken) = Journal::open(dir.path().into()).unwrap();
    assert_eq!(woken.try_recv().unwrap(), ENTRY);
    let job = journal.next(ENTRY).unwrap();
    assert_eq!(job.record, before);
    assert_eq!(fs::read(job.sealed).unwrap(), b"closed");
    assert_eq!(read(&journal), b"closed");
    assert!(!dir.path().join("7.work").exists());
}

// contract: C3
#[test]
fn a_sealed_copy_never_changes_under_the_committer() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _woken) = sealed(dir.path(), b"first");
    let first = journal.next(ENTRY).unwrap();
    let writer = journal.opened(ENTRY);
    journal.write(ENTRY, 5, b" and more").unwrap();
    assert_eq!(read(&journal), b"first and more");
    journal.attach(ENTRY, true).unwrap();
    journal.write(ENTRY, 0, b"second").unwrap();
    journal.seal(ENTRY).unwrap();
    drop(writer);
    assert_eq!(fs::read(&first.sealed).unwrap(), b"first");

    journal.finish(&first, Outcome::Committed(base()));
    assert!(!first.sealed.exists(), "a newer seal replaced it");
    let second = journal.next(ENTRY).unwrap();
    assert_eq!(fs::read(&second.sealed).unwrap(), b"second");
    assert_eq!(read(&journal), b"second");
}

// contract: C1, C2, C4a
#[test]
fn changes_shadow_reads_until_committed_then_the_commit_does() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _woken) = sealed(dir.path(), b"new");
    let reader = journal.opened(ENTRY);
    assert_eq!(journal.pending_count(), 1);

    let job = journal.next(ENTRY).unwrap();
    journal.finish(&job, Outcome::Retrying);
    assert_eq!(read(&journal), b"new");
    let version = Version {
        id: VersionId(4),
        ..base()
    };
    journal.finish(&job, Outcome::Committed(version));
    assert!(
        journal.changes(ENTRY).is_none(),
        "nothing left to shadow the authority"
    );
    assert!(journal.next(ENTRY).is_none());
    drop(reader);
    assert_eq!(journal.pending_count(), 0);
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
    let (journal, _woken) = sealed(dir.path(), b"denied");
    let job = journal.next(ENTRY).unwrap();
    journal.finish(&job, Outcome::Rejected);
    assert!(journal.next(ENTRY).is_none());

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
    let (journal, _woken) = Journal::open(dir.path().into()).unwrap();
    let journal = Arc::new(journal);
    assert!(
        !journal.set_mtime(ENTRY, 1).unwrap(),
        "no copy: set at the authority"
    );
    let writer = journal.opened(ENTRY);
    journal
        .install(ENTRY, journal.scratch(ENTRY).unwrap(), None)
        .unwrap();
    journal.write(ENTRY, 0, b"copied").unwrap();
    assert!(journal.set_mtime(ENTRY, 2).unwrap());
    journal.seal(ENTRY).unwrap();
    drop(writer);
    assert!(
        journal.set_mtime(ENTRY, 3).unwrap(),
        "pending: the record takes it"
    );
    let job = journal.next(ENTRY).unwrap();
    assert_eq!(job.record.mtime_ms, Some(3));
    assert!(
        !journal.set_mtime(ENTRY, 4).unwrap(),
        "under way: set after it lands"
    );
}

// contract: C4a
#[test]
fn a_record_whose_commit_settled_before_a_crash_is_dropped_at_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _woken) = sealed(dir.path(), b"committed");
    let job = journal.next(ENTRY).unwrap();
    drop(journal);
    fs::remove_file(&job.sealed).unwrap();
    let (journal, _woken) = Journal::open(dir.path().into()).unwrap();
    assert!(journal.next(ENTRY).is_none());
    assert!(!dir.path().join("7.json").exists());
}

// contract: none (docs/architecture.md: a file becomes visible to other hosts on close)
#[test]
fn a_file_closed_again_and_again_commits_within_a_second_of_its_first_close() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _woken) = sealed(dir.path(), b"first");
    let _writer = journal.opened(ENTRY);
    std::thread::sleep(Duration::from_millis(900));
    journal.write(ENTRY, 0, b"again").unwrap();
    journal.seal(ENTRY).unwrap();
    assert!(
        journal
            .quiet(ENTRY)
            .is_none_or(|left| left <= Duration::from_millis(100))
    );
}

// contract: C10
#[test]
fn a_removed_entrys_changes_are_dropped_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _woken) = sealed(dir.path(), b"scratch");
    journal.discard(ENTRY);
    assert!(journal.changes(ENTRY).is_none() && journal.next(ENTRY).is_none());
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
    let (journal, _woken) = sealed(dir.path(), b"written");
    let job = journal.next(ENTRY).unwrap();
    let written = fs::metadata(&job.sealed).unwrap().modified().unwrap();
    assert_eq!(job.record.mtime_ms, Some(crate::unix_ms(written)));
}

// contract: C1
#[test]
fn remote_state_from_before_a_settled_commit_is_never_paired_with_the_journal_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _woken) = sealed(dir.path(), b"appended");
    let before = journal.settled();
    let job = journal.next(ENTRY).unwrap();
    let local = journal.changes_as_of(ENTRY, before).unwrap();
    assert!(local.is_some(), "while the commit runs, its copy shows");
    journal.finish(&job, Outcome::Committed(base()));
    assert!(
        journal.changes_as_of(ENTRY, before).is_none(),
        "state asked for before the settle is stale once its copy is gone"
    );
    let after = journal.changes_as_of(ENTRY, journal.settled());
    assert!(after.is_some_and(|local| local.is_none()));
}

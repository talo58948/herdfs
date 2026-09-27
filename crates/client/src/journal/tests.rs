use std::os::unix::fs::FileExt;

use herdfs_core::ChunkRef;

use super::{commit::Outcome, *};

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
    journal.copy(ENTRY).unwrap().write_all_at(bytes, 0).unwrap();
    journal.seal(ENTRY).unwrap();
    drop(registration);
    (journal, woken)
}

// contract: C4a
#[test]
fn a_sealed_write_is_recovered_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, mut woken) = sealed(dir.path(), b"new bytes");
    assert_eq!(woken.try_recv().unwrap(), ENTRY);
    let before = journal.next(ENTRY).unwrap().record;
    assert_eq!(before.base, Some(VersionId(3)));
    assert_eq!(before.base_chunks, [ChunkId::of(b"old")]);
    drop(journal);

    let (journal, mut woken) = Journal::open(dir.path().into()).unwrap();
    assert_eq!(woken.try_recv().unwrap(), ENTRY);
    let job = journal.next(ENTRY).unwrap();
    assert_eq!(job.record, before);
    assert_eq!(fs::read(job.copy).unwrap(), b"new bytes");
}

// contract: C1, C4a
#[test]
fn the_copy_shadows_reads_until_its_commit_settles() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _woken) = sealed(dir.path(), b"new");
    assert!(journal.copy(ENTRY).is_some());
    assert_eq!(journal.pending_count(), 1);

    let job = journal.next(ENTRY).unwrap();
    journal.finish(&job, Outcome::Retrying);
    assert!(journal.copy(ENTRY).is_some());
    let version = Version {
        id: VersionId(4),
        ..base()
    };
    journal.finish(&job, Outcome::Committed(version));
    assert!(journal.next(ENTRY).is_none());
    assert!(journal.copy(ENTRY).is_none());
    assert_eq!(journal.pending_count(), 0);
    assert!(!dir.path().join("7.json").exists() && !dir.path().join("7.copy").exists());
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

// contract: C1
#[test]
fn an_open_handle_keeps_a_committed_copy() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _woken) = sealed(dir.path(), b"x");
    let reader = journal.opened(ENTRY);
    let job = journal.next(ENTRY).unwrap();
    journal.finish(&job, Outcome::Committed(base()));
    assert!(journal.next(ENTRY).is_none());
    assert!(journal.copy(ENTRY).is_some());
    drop(reader);
    assert!(journal.copy(ENTRY).is_none());
}

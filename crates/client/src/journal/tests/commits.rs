//! What a commit carries and how the committer settles each outcome.

use super::*;
use herdfs_core::SpacePath;
use herdfs_core::proto::{self, Entry, Kind};
use std::time::Instant;

// contract: C4a, S8
#[test]
fn a_commit_confirmed_without_a_version_drops_its_journal_copy() {
    let dir = tempfile::tempdir().unwrap();
    let (mount, _woken) = sealed(dir.path(), b"committed");
    let job = mount.committer.take(ENTRY).unwrap();
    let attempts = mount.local(|local, _| local.pending().unwrap().1);

    mount.committer.finish(&job, Outcome::Committed(None));

    assert_eq!(*attempts.borrow(), (job.record.seal, true));
    assert!(mount.changes().is_none());
    assert!(mount.committer.take(ENTRY).is_none());
    assert!(!dir.path().join("7.json").exists());
    assert!(!job.sealed.exists());
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
    mount.committer.finish(&job, Outcome::Committed(Some(committed)));
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
    let sent = commit::commit_dropping_lost_base(record, attempt).await;
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

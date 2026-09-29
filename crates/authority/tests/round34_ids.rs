//! Red team, round 34: protocol ids at the SQLite integer boundary.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_core::proto::{EntryId, ErrorCode, Request, VersionId};
use support::{Fixture, SPACE, alice};

/// Unknown ids within SQLite's integer range get their specified domain errors. The same
/// protocol-valid ids just above that range must not turn into an internal database failure.
// contract: C14 (docs/architecture.md: an unknown entry id says so)
#[ignore = "ROUND34: protocol-valid u64 ids above SQLite's range return Unavailable"]
#[test]
fn unsigned_ids_are_not_database_failures() {
    let mut entry_failures = 0;
    let mut version_failures = 0;
    for trial in 0..3 {
        let mut fx = Fixture::new();
        let (file, version) = fx.file("f", b"head");
        let commit = |entry, base| Request::Commit {
            space: SPACE.into(),
            entry,
            request_id: format!("trial-{trial}-{entry:?}-{base:?}"),
            base,
            guarded: false,
            chunks: version.chunks.clone(),
            mtime_ms: None,
        };

        assert_eq!(
            fx.call(&alice(), commit(EntryId(i64::MAX as u64), Some(version.id))),
            Err(ErrorCode::NotFound),
            "in-range unknown entry control"
        );
        assert_eq!(
            fx.call(&alice(), commit(file.id, Some(VersionId(i64::MAX as u64)))),
            Err(ErrorCode::Conflict),
            "in-range unknown base control"
        );

        let entry = EntryId((i64::MAX as u64) + 1 + trial);
        entry_failures +=
            usize::from(fx.call(&alice(), commit(entry, Some(version.id))) == Err(ErrorCode::Unavailable));
        let base = VersionId((i64::MAX as u64) + 1 + trial);
        version_failures +=
            usize::from(fx.call(&alice(), commit(file.id, Some(base))) == Err(ErrorCode::Unavailable));
    }
    assert_eq!(
        (entry_failures, version_failures),
        (0, 0),
        "oversized entry ids failed {entry_failures}/3 trials and version ids failed {version_failures}/3"
    );
}

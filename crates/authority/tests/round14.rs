//! Red team, round 14: history access boundaries and commit replays.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_core::proto::{EntryId, ErrorCode, Perm, Request, VersionId};
use support::{Fixture, SPACE, alice, bob, path};

fn set_bob_at(fixture: &mut Fixture, perm: Option<Perm>, at: i64) {
    let request = Request::SetAcl {
        space: SPACE.into(),
        path: path("f"),
        principal: bob().user,
        perm,
    };
    fixture.authority.handle(&alice(), request, at).unwrap();
}

/// Makes v1, grants bob read, then makes v2. `after` controls whether the grant follows v1 by
/// that many milliseconds; request order always puts the grant after v1 and before v2.
fn history(after: i64) -> (VersionId, Vec<VersionId>) {
    let mut fixture = Fixture::new();
    let (file, first) = fixture.file("f", b"first");
    set_bob_at(&mut fixture, Some(Perm::Read), first.created_ms + after);
    fixture
        .commit(&alice(), file.id, "second", None, false, &[])
        .unwrap();
    let history = fixture.history(&bob(), "f").unwrap();
    (first.id, history.into_iter().map(|version| version.id).collect())
}

/// Control: a version created one millisecond before the grant stays hidden after a newer commit.
// contract: S8
#[test]
fn a_grant_hides_history_from_an_earlier_millisecond() {
    let (first, history) = history(1);
    assert!(!history.contains(&first), "the pre-grant version is hidden");
}

/// Repro: v1 commits immediately before bob's read grant at the same millisecond, then v2
/// commits. S8 says only versions created after access are visible, but history includes v1.
// contract: S8
#[test]
fn a_grant_hides_history_created_earlier_in_the_same_millisecond() {
    let (first, history) = history(0);
    assert!(!history.contains(&first), "the pre-grant version is hidden");
}

fn unguarded(entry: EntryId, request_id: &str) -> Request {
    Request::Commit {
        space: SPACE.into(),
        entry,
        request_id: request_id.into(),
        base: None,
        guarded: false,
        chunks: Vec::new(),
        mtime_ms: None,
    }
}

/// Repro: bob commits while he has write access, alice commits a newer head, then revokes him. A
/// fresh commit is denied, but replaying bob's request id returns its retained older version
/// without checking current permission, confirming a version bob can no longer see.
// contract: S8
#[test]
fn a_revoked_writer_cannot_confirm_a_version_by_replaying_its_commit() {
    let mut fixture = Fixture::new();
    let (file, _) = fixture.file("f", b"initial");
    fixture.set_acl("f", "user:bob", Some(Perm::Write));
    let _first = fixture.commit(&bob(), file.id, "mine", None, false, &[]).unwrap();
    fixture
        .commit(&alice(), file.id, "newer", None, false, &[])
        .unwrap();
    fixture.set_acl("f", "user:bob", None);

    assert_eq!(
        fixture.commit(&bob(), file.id, "fresh", None, false, &[]),
        Err(ErrorCode::Denied),
        "control: a fresh commit checks current permission"
    );
    let replay = fixture
        .authority
        .handle(&bob(), unguarded(file.id, "mine"), i64::MAX);
    assert_eq!(replay.map_err(|error| error.code), Err(ErrorCode::Denied));
}

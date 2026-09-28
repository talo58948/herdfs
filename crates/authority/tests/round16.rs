//! Red team, round 16 probes: S8 by commit sequence and commit replays.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_core::proto::{ErrorCode, Perm, Request, Response, VersionId};
use support::{Fixture, SPACE, alice, bob, carol, path, user};

/// The ids of `versions`, in order.
fn ids(versions: &[herdfs_core::proto::Version]) -> Vec<VersionId> {
    versions.iter().map(|version| version.id).collect()
}

/// A grant made while the authority holds no version at all counts from sequence 0, so every
/// later version of the entry is in the grantee's history.
// contract: S8
#[test]
fn a_grant_before_any_version_shows_all_later_history() {
    let mut fixture = Fixture::new();
    fixture.mkdir(&alice(), "d").unwrap();
    fixture.set_acl("d", "user:bob", Some(Perm::Read));
    let (file, first) = fixture.file("d/f", b"one");
    let second = fixture
        .commit(&alice(), file.id, "two", None, false, &[])
        .unwrap();
    let history = fixture.history(&bob(), "d/f").unwrap();
    assert_eq!(ids(&history), [first.id, second.id]);
}

/// Removing the newest version anywhere lowers `max(id)` below the AUTOINCREMENT counter before
/// bob's grant. Versions made before the grant stay hidden from history, grant, restore and a
/// guarded base; the next version, whose id skips the gap, is visible.
// contract: S8
#[test]
fn a_grant_after_the_newest_version_was_removed_hides_older_history() {
    let mut fixture = Fixture::new();
    let (file, first) = fixture.file("f", b"one");
    let (_, newest) = fixture.file("g", b"newest");
    fixture.remove(&alice(), "g").unwrap();
    fixture.set_acl("f", "user:bob", Some(Perm::Read));
    let second = fixture
        .commit(&alice(), file.id, "two", None, false, &[])
        .unwrap();
    assert!(second.id > newest.id);
    let history = fixture.history(&bob(), "f").unwrap();
    assert_eq!(ids(&history), [second.id]);
    assert_eq!(
        fixture.grant(&bob(), file.id, first.id, &[0]),
        Err(ErrorCode::NotFound)
    );
    let restore = Request::Restore {
        space: SPACE.into(),
        path: path("f"),
        version: first.id,
    };
    fixture.set_acl("f", "user:bob", Some(Perm::Write));
    assert_eq!(fixture.call(&bob(), restore), Err(ErrorCode::NotFound));
    let based = fixture.commit(&bob(), file.id, "based", Some(first.id), true, &[]);
    assert_eq!(based, Err(ErrorCode::Conflict));
}

/// A group grant counts from when the group was granted; a later user grant for a member does
/// not narrow or widen that.
// contract: S8
#[test]
fn a_group_grant_counts_from_the_group_and_a_later_user_grant_changes_nothing() {
    let mut fixture = Fixture::new();
    let (file, first) = fixture.file("f", b"one");
    fixture.set_acl("f", "group:eng", Some(Perm::Read));
    let second = fixture
        .commit(&alice(), file.id, "two", None, false, &[])
        .unwrap();
    let dave = user("dave", &["eng"]);
    fixture.set_acl("f", "user:dave", Some(Perm::Write));
    let third = fixture
        .commit(&alice(), file.id, "three", None, false, &[])
        .unwrap();
    let history = fixture.history(&dave, "f").unwrap();
    assert_eq!(ids(&history), [second.id, third.id]);
    assert!(!ids(&history).contains(&first.id));
}

/// The fork copies each head as a new version row with a new id. A grant made in the fork after
/// it hides that copy once a newer version supersedes it.
// contract: S8, C13
#[test]
fn a_grant_in_a_fork_hides_the_versions_the_fork_copied() {
    let mut fixture = Fixture::new();
    let (file, _) = fixture.file("f", b"one");
    fixture
        .commit(&alice(), file.id, "two", None, false, &[])
        .unwrap();
    let fork = Request::Fork {
        space: SPACE.into(),
        name: "copy".into(),
    };
    fixture.call(&alice(), fork).unwrap();
    let grant = Request::SetAcl {
        space: "copy".into(),
        path: path("f"),
        principal: "user:carol".into(),
        perm: Some(Perm::Read),
    };
    fixture.call(&alice(), grant).unwrap();
    let Response::Entry { entry } = fixture
        .call(
            &alice(),
            Request::Stat {
                space: "copy".into(),
                path: path("f"),
            },
        )
        .unwrap()
    else {
        panic!("stat answers with an entry")
    };
    let copied = entry.head.unwrap();
    let commit = Request::Commit {
        space: "copy".into(),
        entry: entry.id,
        request_id: "fork-three".into(),
        base: None,
        guarded: false,
        chunks: Vec::new(),
        mtime_ms: None,
    };
    fixture.call(&alice(), commit).unwrap();
    let Response::History { versions } = fixture
        .call(
            &carol(),
            Request::History {
                space: "copy".into(),
                path: path("f"),
            },
        )
        .unwrap()
    else {
        panic!("history")
    };
    assert!(
        !ids(&versions).contains(&copied),
        "the copied head is older than carol's grant"
    );
    assert_eq!(versions.len(), 1);
}

/// Bob's commit replayed after his entry moved out of his reach answers his retained version;
/// after a purge removed it, `Done`; once he may read the new place, the head; once the entry is
/// removed, `NotFound`.
// contract: S8, C4a
#[test]
fn a_replay_after_rename_purge_and_revocation_answers_done() {
    let mut fixture = Fixture::new();
    fixture.mkdir(&alice(), "a").unwrap();
    fixture.mkdir(&alice(), "b").unwrap();
    let (file, _) = fixture.file("a/f", b"one");
    fixture.set_acl("a", "user:bob", Some(Perm::Write));
    let mine = fixture.commit(&bob(), file.id, "bob", None, false, &[]).unwrap();
    fixture
        .commit(&alice(), file.id, "later", None, false, &[])
        .unwrap();
    fixture.rename(&alice(), "a/f", "b/f").unwrap();
    let replay = |fixture: &mut Fixture| {
        fixture.call(
            &bob(),
            Request::Commit {
                space: SPACE.into(),
                entry: file.id,
                request_id: "bob".into(),
                base: None,
                guarded: false,
                chunks: Vec::new(),
                mtime_ms: None,
            },
        )
    };
    assert_eq!(
        replay(&mut fixture),
        Ok(Response::Version {
            version: mine.clone()
        })
    );
    let purge = Request::Purge {
        space: SPACE.into(),
        path: path("b/f"),
    };
    fixture.call(&alice(), purge).unwrap();
    assert_eq!(replay(&mut fixture), Ok(Response::Done));
    fixture.set_acl("b", "user:bob", Some(Perm::Read));
    let Ok(Response::Version { version }) = replay(&mut fixture) else {
        panic!("a readable head answers")
    };
    assert_ne!(version.id, mine.id);
    fixture.remove(&alice(), "b/f").unwrap();
    assert_eq!(replay(&mut fixture), Err(ErrorCode::NotFound));
}

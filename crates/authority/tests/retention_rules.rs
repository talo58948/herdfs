//! Red team, round 8: history since access with inherited grants (S8), restore after a late
//! grant, and commits or forks between a collection and its sweep (C14), on the authority alone.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use std::collections::BTreeSet;

use herdfs_authority::Policy;
use herdfs_core::{
    ChunkId, ChunkRef,
    proto::{EntryId, ErrorCode, Perm, Request, Response, Version, VersionId},
};
use support::{Fixture, SPACE, alice, bob, path, user};

/// Keeps each entry's head only, and no upload.
const HEAD_ONLY: Policy = Policy {
    versions: 1,
    max_age_ms: 0,
    grace_ms: 0,
};

/// Commits `bytes` to `entry` as alice, as one chunk she uploads first.
fn write(fx: &mut Fixture, entry: EntryId, request_id: &str, bytes: &[u8]) -> Version {
    let chunk = fx.upload(&alice(), bytes).unwrap();
    fx.commit(&alice(), entry, request_id, None, false, &[chunk])
        .unwrap()
}

fn restore(at: &str, version: VersionId) -> Request {
    Request::Restore {
        space: SPACE.into(),
        path: path(at),
        version,
    }
}

/// The chunks of every version of `at` in `space`, all of which alice sees as an admin.
fn chunks_in_history(fx: &mut Fixture, space: &str, at: &str) -> Vec<ChunkId> {
    let request = Request::History {
        space: space.into(),
        path: path(at),
    };
    let Ok(Response::History { versions }) = fx.call(&alice(), request) else {
        panic!("alice reads the history of {at} in {space}");
    };
    versions
        .iter()
        .flat_map(|version| &version.chunks)
        .map(|chunk| chunk.id)
        .collect()
}

// S8: history counts from the earliest current grant on the path or an ancestor.

#[test]
fn an_old_grant_on_the_root_shows_history_made_after_it() {
    // contract: S8
    let mut fx = Fixture::new();
    fx.set_acl("", "user:bob", Some(Perm::Read));
    fx.mkdir(&alice(), "proj").unwrap();
    let (file, v1) = fx.file("proj/f", b"one");
    let v2 = write(&mut fx, file.id, "r2", b"two");
    assert_eq!(fx.history(&bob(), "proj/f").unwrap(), [v1.clone(), v2]);
    assert_eq!(fx.grant(&bob(), file.id, v1.id, &[0]).unwrap(), [v1.chunks[0].id]);
}

/// bob has read on `other` from the start and on `proj` only after v1 and v2 of `proj/f`. When
/// alice moves the file under `other`, its history follows the path: moving an entry is its
/// mover's disclosure decision (S8), so bob now sees v1 and v2.
#[test]
fn moving_an_entry_under_an_older_grant_shows_the_history_that_grant_covers() {
    // contract: S8
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "other").unwrap();
    fx.set_acl("other", "user:bob", Some(Perm::Read));
    fx.mkdir(&alice(), "proj").unwrap();
    let (file, v1) = fx.file("proj/f", b"one");
    let v2 = write(&mut fx, file.id, "r2", b"two");
    fx.set_acl("proj", "user:bob", Some(Perm::Read));
    assert_eq!(fx.history(&bob(), "proj/f").unwrap(), std::slice::from_ref(&v2));

    fx.rename(&alice(), "proj/f", "other/f").unwrap();
    assert_eq!(fx.history(&bob(), "other/f").unwrap(), [v1.clone(), v2]);
    assert_eq!(fx.grant(&bob(), file.id, v1.id, &[0]).unwrap(), [v1.chunks[0].id]);
}

/// The converse of the move above, pinned as it stands: history follows the grants of the
/// entry's current path, so a move away from bob's old grant hides versions he could read.
#[test]
fn moving_an_entry_away_from_an_older_grant_hides_its_earlier_history() {
    // contract: S8, C14
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "proj").unwrap();
    fx.set_acl("proj", "user:bob", Some(Perm::Read));
    let (file, v1) = fx.file("proj/f", b"one");
    let v2 = write(&mut fx, file.id, "r2", b"two");
    assert_eq!(fx.history(&bob(), "proj/f").unwrap(), [v1.clone(), v2.clone()]);

    fx.mkdir(&alice(), "other").unwrap();
    fx.set_acl("other", "user:bob", Some(Perm::Read));
    fx.rename(&alice(), "proj/f", "other/f").unwrap();
    assert_eq!(fx.history(&bob(), "other/f").unwrap(), [v2]);
    assert_eq!(fx.grant(&bob(), file.id, v1.id, &[0]), Err(ErrorCode::NotFound));
}

/// v1, then eng gets read on `proj`, v2, then bob gets his own read on the file, v3, v4.
/// Holding both rows, bob sees from the group's grant; holding his own only, from his.
#[test]
fn a_group_grant_older_than_the_users_own_counts_from_the_group_grant() {
    // contract: S8
    let mut fx = Fixture::new();
    let bob_in_eng = user("bob", &["eng"]);
    fx.mkdir(&alice(), "proj").unwrap();
    let (file, _v1) = fx.file("proj/f", b"one");
    fx.set_acl("proj", "group:eng", Some(Perm::Read));
    let v2 = write(&mut fx, file.id, "r2", b"two");
    fx.set_acl("proj/f", "user:bob", Some(Perm::Read));
    let v3 = write(&mut fx, file.id, "r3", b"three");
    let v4 = write(&mut fx, file.id, "r4", b"four");

    assert_eq!(
        fx.history(&bob_in_eng, "proj/f").unwrap(),
        [v2.clone(), v3.clone(), v4.clone()]
    );
    assert_eq!(fx.history(&bob(), "proj/f").unwrap(), [v3, v4]);
    assert_eq!(fx.grant(&bob(), file.id, v2.id, &[0]), Err(ErrorCode::NotFound));
}

/// eng has read on `proj` before v1 and v2 of `proj/f`. bob, in no group, is refused the
/// history; once his token names eng, the group's grant time counts (S8): membership belongs to
/// the identity provider, which keeps no join time the authority could use.
#[test]
fn a_group_grant_counts_from_when_the_group_was_granted() {
    // contract: S8
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "proj").unwrap();
    fx.set_acl("proj", "group:eng", Some(Perm::Read));
    let (file, v1) = fx.file("proj/f", b"one");
    let v2 = write(&mut fx, file.id, "r2", b"two");
    assert_eq!(fx.history(&bob(), "proj/f"), Err(ErrorCode::Denied));

    let joined = user("bob", &["eng"]);
    assert_eq!(fx.history(&joined, "proj/f").unwrap(), [v1.clone(), v2]);
    assert_eq!(
        fx.grant(&joined, file.id, v1.id, &[0]).unwrap(),
        [v1.chunks[0].id]
    );
}

#[test]
fn a_late_writer_cannot_restore_or_see_earlier_versions() {
    // contract: S8, C14
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"the first plan, longer");
    let v2 = write(&mut fx, file.id, "r2", b"second plan");
    fx.set_acl("f", "user:bob", Some(Perm::Write));
    assert_eq!(fx.call(&bob(), restore("f", v1.id)), Err(ErrorCode::NotFound));

    // Open and Locate describe the head alone: its id, size and time, nothing of v1.
    let open = Request::Open {
        space: SPACE.into(),
        entry: file.id,
    };
    let Response::Opened {
        entry,
        version,
        writable,
    } = fx.call(&bob(), open).unwrap()
    else {
        panic!("open answers with the entry");
    };
    assert_eq!(version.as_ref(), Some(&v2));
    assert!(writable);
    let described = (entry.head, entry.size, entry.mtime_ms);
    assert_eq!(described, (Some(v2.id), v2.size, v2.created_ms));
    let locate = Request::Locate {
        space: SPACE.into(),
        entry: file.id,
    };
    assert_eq!(fx.call(&bob(), locate), Ok(Response::Entry { entry }));
    assert_eq!(fx.history(&bob(), "f").unwrap(), std::slice::from_ref(&v2));

    // Restoring the head is allowed, and the old head then leaves bob's view.
    let Response::Version { version: v3 } = fx.call(&bob(), restore("f", v2.id)).unwrap() else {
        panic!("restore answers with the new head");
    };
    assert_eq!(v3.chunks, v2.chunks);
    assert_eq!(fx.history(&bob(), "f").unwrap(), [v3]);
    assert_eq!(fx.grant(&bob(), file.id, v2.id, &[0]), Err(ErrorCode::NotFound));
}

/// bob gets write on `f` after v1 and v2. A commit on base v1 must not take v1's chunks without an
/// upload, nor tell a right guess of v1's bytes from a wrong one: a hidden base answers `Conflict`,
/// as a missing one does, whatever the chunks.
#[test]
fn a_late_writer_cannot_commit_on_a_base_from_before_his_grant() {
    // contract: S6, S8
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"salary: 100");
    write(&mut fx, file.id, "r2", b"salary: 120");
    fx.set_acl("f", "user:bob", Some(Perm::Write));
    assert_eq!(fx.grant(&bob(), file.id, v1.id, &[0]), Err(ErrorCode::NotFound));

    // bob names a chunk by hashing bytes he guesses; he uploads nothing.
    let mut on_v1 = |request_id: &str, guess: &[u8]| {
        let len = u32::try_from(guess.len()).unwrap();
        let chunks = [ChunkRef {
            id: ChunkId::of(guess),
            len,
        }];
        fx.commit(&bob(), file.id, request_id, Some(v1.id), false, &chunks)
    };
    let wrong = on_v1("wrong", b"salary: 90");
    let right = on_v1("right", b"salary: 100");
    assert_eq!(right, Err(ErrorCode::Conflict), "bob made v1's bytes the head");
    assert_eq!(right, wrong, "the answer tells whether v1 held the guess");
}

// C14: between a collection and its sweep, nothing may come to reference a chunk outside the
// live set that the sweep will not check again.

#[test]
fn after_a_collection_nothing_reaches_a_pruned_versions_chunks() {
    // contract: C14, C15
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"one");
    let v2 = write(&mut fx, file.id, "r2", b"two");
    let (other, g1) = fx.file("g", b"three");
    let collected = fx.authority.collect(g1.created_ms + 1, &HEAD_ONLY).unwrap();
    assert_eq!(collected.pruned, 1);
    let pruned = v1.chunks[0];
    assert!(!collected.live.contains(&pruned.id));

    let mut commit =
        |entry, request_id: &str, base| fx.commit(&alice(), entry, request_id, base, false, &[pruned]);
    assert_eq!(commit(file.id, "on-v1", Some(v1.id)), Err(ErrorCode::Conflict));
    assert_eq!(commit(file.id, "on-v2", Some(v2.id)), Err(ErrorCode::Invalid));
    assert_eq!(commit(other.id, "elsewhere", None), Err(ErrorCode::Invalid));
    // The request id that made v1 answers with the head, never with v1.
    assert_eq!(commit(file.id, "f", None).map(|version| version.id), Ok(v2.id));
    assert_eq!(fx.call(&alice(), restore("f", v1.id)), Err(ErrorCode::NotFound));

    // A fork copies each head only, so neither space names the pruned chunk.
    let fork = Request::Fork {
        space: SPACE.into(),
        name: "copy".into(),
    };
    fx.call(&alice(), fork).unwrap();
    let mut referenced = BTreeSet::new();
    for (space, at) in [(SPACE, "f"), (SPACE, "g"), ("copy", "f"), ("copy", "g")] {
        referenced.extend(chunks_in_history(&mut fx, space, at));
    }
    assert!(referenced.is_subset(&collected.live), "{referenced:?}");
}

/// A committed chunk outside an earlier live-set snapshot came from an upload after collection.
#[test]
fn after_a_collection_a_chunk_outside_the_live_set_comes_only_from_a_fresh_upload() {
    // contract: C14, C15
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"one");
    let v2 = write(&mut fx, file.id, "r2", b"two");
    let started = v2.created_ms + 1;
    let collected = fx.authority.collect(started, &HEAD_ONLY).unwrap();
    assert!(!collected.live.contains(&v1.chunks[0].id));

    let again = fx.upload(&alice(), b"one").unwrap();
    let new = fx.upload(&alice(), b"four").unwrap();
    let chunks = [again, new, v2.chunks[0]];
    let v3 = fx
        .commit(&alice(), file.id, "r3", Some(v2.id), false, &chunks)
        .unwrap();
    let outside: BTreeSet<_> = v3
        .chunks
        .iter()
        .map(|chunk| chunk.id)
        .filter(|id| !collected.live.contains(id))
        .collect();
    assert_eq!(outside, BTreeSet::from([again.id, new.id]));
}

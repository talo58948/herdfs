//! Commits, grants, history, restore and forks.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_core::proto::{EntryId, ErrorCode, Perm, Request, Response, VersionId};
use support::{Fixture, SPACE, alice, bob, carol, path};

#[test]
fn commits_reference_only_uploaded_or_base_chunks() {
    // contract: S6
    let mut fx = Fixture::new();
    fx.set_acl(&alice(), "", "user:bob", Some(Perm::Write)).unwrap();
    let file = fx.create(&alice(), "f").unwrap();
    let mine = fx.upload(&alice(), b"mine").unwrap();
    let theirs = fx.upload(&bob(), b"theirs").unwrap();
    assert_eq!(
        fx.commit(&alice(), file.id, "r1", None, false, &[theirs]),
        Err(ErrorCode::Invalid)
    );
    let v1 = fx.commit(&alice(), file.id, "r2", None, false, &[mine]).unwrap();

    // bob never uploaded `mine`, but it is in his base.
    assert_eq!(
        fx.commit(&bob(), file.id, "r3", None, false, &[mine]),
        Err(ErrorCode::Invalid)
    );
    let v2 = fx
        .commit(&bob(), file.id, "r4", Some(v1.id), false, &[theirs, mine])
        .unwrap();
    assert_eq!((v2.size, v2.author.as_str()), (10, "user:bob"));

    let (_, other) = fx.file("g", b"other");
    assert_eq!(
        fx.commit(&alice(), file.id, "r5", Some(other.id), false, &[mine]),
        Err(ErrorCode::Invalid)
    );
    assert_eq!(
        fx.commit(&alice(), EntryId(9_999), "r6", None, false, &[]),
        Err(ErrorCode::NotFound)
    );
    let dir = fx.mkdir(&alice(), "d").unwrap();
    assert_eq!(
        fx.commit(&alice(), dir.id, "r7", None, false, &[]),
        Err(ErrorCode::Invalid)
    );
}

#[test]
fn unguarded_commits_keep_every_version_and_the_last_is_head() {
    // contract: C5, C3
    let mut fx = Fixture::new();
    let file = fx.create(&alice(), "f").unwrap();
    let one = fx.upload(&alice(), b"one").unwrap();
    let two = fx.upload(&alice(), b"second").unwrap();
    let v1 = fx.commit(&alice(), file.id, "r1", None, false, &[one]).unwrap();
    let v2 = fx
        .commit(&alice(), file.id, "r2", None, false, &[two, one])
        .unwrap();
    let head = fx.stat(&alice(), "f").unwrap();
    assert_eq!(
        (head.head, head.size, head.mtime_ms),
        (Some(v2.id), 9, v2.created_ms)
    );
    assert_eq!(fx.history(&alice(), "f").unwrap(), [v1, v2.clone()]);
    let opened = fx
        .call(
            &alice(),
            Request::Open {
                space: SPACE.into(),
                path: path("f"),
            },
        )
        .unwrap();
    assert_eq!(
        opened,
        Response::Opened {
            entry: head,
            version: Some(v2)
        }
    );
}

#[test]
fn guarded_commits_on_one_base_have_one_winner() {
    // contract: C6
    let mut fx = Fixture::new();
    let file = fx.create(&alice(), "f").unwrap();
    let chunk = fx.upload(&alice(), b"x").unwrap();
    let v1 = fx.commit(&alice(), file.id, "r1", None, true, &[chunk]).unwrap();
    assert_eq!(
        fx.commit(&alice(), file.id, "r2", None, true, &[chunk]),
        Err(ErrorCode::Conflict)
    );
    fx.commit(&alice(), file.id, "r3", Some(v1.id), true, &[chunk])
        .unwrap();
    assert_eq!(
        fx.commit(&alice(), file.id, "r4", Some(v1.id), true, &[chunk]),
        Err(ErrorCode::Conflict)
    );
    assert_eq!(fx.history(&alice(), "f").unwrap().len(), 2);
}

#[test]
fn a_repeated_request_id_returns_the_first_version() {
    // contract: C4a, C12
    let mut fx = Fixture::new();
    fx.set_acl(&alice(), "", "user:bob", Some(Perm::Write)).unwrap();
    let file = fx.create(&alice(), "f").unwrap();
    let chunk = fx.upload(&alice(), b"x").unwrap();
    let first = fx
        .commit(&alice(), file.id, "same", None, true, &[chunk])
        .unwrap();
    assert_eq!(
        fx.commit(&alice(), file.id, "same", None, true, &[chunk]),
        Ok(first.clone())
    );
    fx.reopen();
    assert_eq!(
        fx.commit(&alice(), file.id, "same", None, false, &[]),
        Ok(first.clone())
    );
    assert_eq!(fx.history(&alice(), "f").unwrap(), [first]);
    // Request ids belong to their principal.
    fx.commit(&bob(), file.id, "same", None, false, &[]).unwrap();
    assert_eq!(fx.history(&alice(), "f").unwrap().len(), 2);
}

#[test]
fn grants_cover_chunks_of_a_version_of_that_entry() {
    // contract: S2
    let mut fx = Fixture::new();
    let file = fx.create(&alice(), "f").unwrap();
    let chunks = [
        fx.upload(&alice(), b"a").unwrap(),
        fx.upload(&alice(), b"b").unwrap(),
    ];
    let version = fx.commit(&alice(), file.id, "r1", None, false, &chunks).unwrap();
    assert_eq!(
        fx.grant(&alice(), "f", version.id, &[1, 0, 1]),
        Ok(vec![chunks[1].id, chunks[0].id, chunks[1].id])
    );
    assert_eq!(fx.grant(&alice(), "f", version.id, &[2]), Err(ErrorCode::Invalid));
    let (_, other) = fx.file("g", b"other");
    assert_eq!(fx.grant(&alice(), "f", other.id, &[0]), Err(ErrorCode::NotFound));
    assert_eq!(
        fx.grant(&alice(), "zz", version.id, &[0]),
        Err(ErrorCode::NotFound)
    );
    assert_eq!(fx.grant(&bob(), "f", version.id, &[0]), Err(ErrorCode::Denied));
}

#[test]
fn restore_makes_an_old_version_the_new_head_and_keeps_history() {
    // contract: C14
    let mut fx = Fixture::new();
    let (_, v1) = fx.file("f", b"one");
    let file = fx.stat(&alice(), "f").unwrap();
    let two = fx.upload(&alice(), b"two").unwrap();
    let v2 = fx
        .commit(&alice(), file.id, "r2", Some(v1.id), false, &[two])
        .unwrap();
    let restore = |version| Request::Restore {
        space: SPACE.into(),
        path: path("f"),
        version,
    };
    let Response::Version { version: v3 } = fx.call(&alice(), restore(v1.id)).unwrap() else {
        panic!("restore answers with the new version");
    };
    assert_eq!((v3.chunks.clone(), v3.entry), (v1.chunks.clone(), file.id));
    assert!(v3.id > v2.id);
    assert_eq!(fx.stat(&alice(), "f").unwrap().head, Some(v3.id));
    assert_eq!(fx.history(&alice(), "f").unwrap(), [v1, v2, v3]);
    let (_, other) = fx.file("g", b"other");
    assert_eq!(fx.call(&alice(), restore(other.id)), Err(ErrorCode::NotFound));
    assert_eq!(
        fx.call(&alice(), restore(VersionId(9_999))),
        Err(ErrorCode::NotFound)
    );
}

#[test]
fn a_fork_copies_heads_and_acls_and_is_isolated() {
    // contract: C13
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "d").unwrap();
    let (file, v1) = fx.file("d/f", b"one");
    let two = fx.upload(&alice(), b"two").unwrap();
    let v2 = fx
        .commit(&alice(), file.id, "r2", Some(v1.id), false, &[two])
        .unwrap();
    fx.set_acl(&alice(), "d", "group:eng", Some(Perm::Read)).unwrap();
    let fork = |name: &str| Request::Fork {
        space: SPACE.into(),
        name: name.into(),
    };
    assert_eq!(fx.call(&carol(), fork("copy")), Err(ErrorCode::Denied));
    assert_eq!(fx.call(&alice(), fork("Copy")), Err(ErrorCode::Invalid));
    assert_eq!(fx.call(&alice(), fork(SPACE)), Err(ErrorCode::Exists));
    fx.call(&alice(), fork("copy")).unwrap();

    let in_copy = |at: &str| Request::History {
        space: "copy".into(),
        path: path(at),
    };
    let Response::History { versions } = fx.call(&carol(), in_copy("d/f")).unwrap() else {
        panic!()
    };
    let [copied] = versions.as_slice() else {
        panic!("a fork keeps only heads: {versions:?}")
    };
    assert_ne!((copied.id, copied.entry), (v2.id, file.id));
    assert_eq!((&copied.chunks, copied.size), (&v2.chunks, v2.size));

    // Commits on either side stay on that side.
    let three = fx.upload(&alice(), b"three").unwrap();
    fx.commit(&alice(), file.id, "r3", Some(v2.id), false, &[three])
        .unwrap();
    let Response::History { versions } = fx.call(&carol(), in_copy("d/f")).unwrap() else {
        panic!()
    };
    assert_eq!(versions.len(), 1);
    let request = Request::Commit {
        space: "copy".into(),
        entry: copied.entry,
        request_id: "r4".into(),
        base: Some(copied.id),
        guarded: true,
        chunks: vec![three],
    };
    fx.call(&alice(), request).unwrap();
    assert_eq!(fx.history(&alice(), "d/f").unwrap().len(), 3);
}

#[test]
fn a_fork_of_a_missing_space_is_not_found() {
    // contract: C13
    let mut fx = Fixture::new();
    let request = Request::Fork {
        space: "nowhere".into(),
        name: "copy".into(),
    };
    assert_eq!(fx.call(&alice(), request), Err(ErrorCode::NotFound));
}

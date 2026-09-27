//! Examples of the commit, grant, restore and fork rules. The sim checks them at scale.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_core::{
    ChunkRef,
    proto::{ErrorCode, Perm, Request, Response},
};
use support::{Fixture, SPACE, alice, bob, path};

#[test]
fn commits_reference_chunks_uploaded_by_the_caller_or_in_the_base_at_their_length() {
    // contract: S6
    let mut fx = Fixture::new();
    fx.set_acl("", "user:bob", Some(Perm::Write));
    let (file, v1) = fx.file("f", b"alice's");
    let [alices] = v1.chunks[..] else { panic!() };
    let bobs = fx.upload(&bob(), b"bob's").unwrap();
    assert_eq!(
        fx.commit(&bob(), file.id, "r1", None, false, &[alices]),
        Err(ErrorCode::Invalid)
    );
    let wrong = ChunkRef { len: 99, ..bobs };
    assert_eq!(
        fx.commit(&bob(), file.id, "r2", None, false, &[wrong]),
        Err(ErrorCode::Invalid)
    );
    assert!(
        fx.commit(&bob(), file.id, "r3", Some(v1.id), false, &[alices, bobs])
            .is_ok()
    );
}

#[test]
fn the_last_unguarded_commit_is_head_and_every_commit_stays_in_history() {
    // contract: C5
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"one");
    let v2 = fx.commit(&alice(), file.id, "r2", None, false, &[]).unwrap();
    let head = fx.stat(&alice(), "f").unwrap();
    assert_eq!(
        (head.head, head.size, head.mtime_ms),
        (Some(v2.id), 0, v2.created_ms)
    );
    assert_eq!(fx.history(&alice(), "f").unwrap(), [v1, v2]);
}

#[test]
fn guarded_commits_on_one_base_have_one_winner() {
    // contract: C6
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"one");
    assert!(fx.commit(&alice(), file.id, "r2", Some(v1.id), true, &[]).is_ok());
    assert_eq!(
        fx.commit(&alice(), file.id, "r3", Some(v1.id), true, &[]),
        Err(ErrorCode::Conflict)
    );
}

#[test]
fn a_repeated_request_id_from_the_same_principal_returns_the_first_version() {
    // contract: C4a, C12
    let mut fx = Fixture::new();
    fx.set_acl("", "user:bob", Some(Perm::Write));
    let file = fx.create(&alice(), "f").unwrap();
    let first = fx.commit(&alice(), file.id, "same", None, false, &[]).unwrap();
    assert_eq!(
        fx.commit(&alice(), file.id, "same", None, false, &[]),
        Ok(first.clone())
    );
    let bobs = fx.commit(&bob(), file.id, "same", None, false, &[]).unwrap();
    assert_eq!(fx.history(&alice(), "f").unwrap(), [first, bobs]);
}

#[test]
fn a_request_id_reused_on_another_entry_is_invalid() {
    // contract: C4a, C12
    let mut fx = Fixture::new();
    let (_, version) = fx.file("f", b"one");
    let other = fx.create(&alice(), "g").unwrap();
    let reused = fx.commit(&alice(), other.id, "f", None, false, &[]);
    assert_eq!(
        reused,
        Err(ErrorCode::Invalid),
        "f's commit used request id \"f\""
    );
    assert_eq!(fx.history(&alice(), "g"), Ok(vec![]));
    assert_eq!(fx.history(&alice(), "f"), Ok(vec![version]));
}

#[test]
fn grants_cover_chunks_of_a_version_of_that_entry() {
    // contract: S2
    let mut fx = Fixture::new();
    let (_, version) = fx.file("f", b"one");
    let (_, other) = fx.file("g", b"two");
    assert_eq!(
        fx.grant(&alice(), "f", version.id, &[0]),
        Ok(vec![version.chunks[0].id])
    );
    assert_eq!(fx.grant(&alice(), "f", version.id, &[1]), Err(ErrorCode::Invalid));
    assert_eq!(fx.grant(&alice(), "f", other.id, &[0]), Err(ErrorCode::NotFound));
}

#[test]
fn restore_makes_an_old_version_the_new_head_and_keeps_history() {
    // contract: C14
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"one");
    let v2 = fx.commit(&alice(), file.id, "r2", None, false, &[]).unwrap();
    let restore = Request::Restore {
        space: SPACE.into(),
        path: path("f"),
        version: v1.id,
    };
    let Ok(Response::Version { version: v3 }) = fx.call(&alice(), restore) else {
        panic!()
    };
    assert_eq!(
        (&v3.chunks, fx.stat(&alice(), "f").unwrap().head),
        (&v1.chunks, Some(v3.id))
    );
    assert_eq!(fx.history(&alice(), "f").unwrap(), [v1, v2, v3]);
}

#[test]
fn a_fork_copies_heads_and_is_isolated() {
    // contract: C13
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"one");
    fx.call(
        &alice(),
        Request::Fork {
            space: SPACE.into(),
            name: "copy".into(),
        },
    )
    .unwrap();
    fx.commit(&alice(), file.id, "r2", Some(v1.id), false, &[])
        .unwrap();
    let history = Request::History {
        space: "copy".into(),
        path: path("f"),
    };
    let Ok(Response::History { versions }) = fx.call(&alice(), history) else {
        panic!()
    };
    let [copy] = &versions[..] else {
        panic!("a fork keeps only heads: {versions:?}")
    };
    assert_eq!(copy.chunks, v1.chunks);
    assert_ne!((copy.id, copy.entry), (v1.id, file.id));
}

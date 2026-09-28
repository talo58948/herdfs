//! Examples of retention, purging, deleting spaces and history since grant.
//! The sim checks them at scale.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_authority::Policy;
use herdfs_core::proto::{ErrorCode, Perm, Request};
use support::{Fixture, SPACE, alice, bob, path};

#[test]
fn history_from_before_a_grant_stays_hidden() {
    // contract: S8
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"one");
    let v2 = fx.commit(&alice(), file.id, "r2", None, false, &[]).unwrap();
    fx.set_acl("f", "user:bob", Some(Perm::Read));
    let v3 = fx.commit(&alice(), file.id, "r3", None, false, &[]).unwrap();
    assert_eq!(fx.history(&bob(), "f").unwrap(), std::slice::from_ref(&v3));
    // A hidden version answers as an unknown one would.
    assert_eq!(fx.grant(&bob(), file.id, v1.id, &[0]), Err(ErrorCode::NotFound));
    // A perm change keeps the grant time; admins of the root see everything.
    fx.set_acl("f", "user:bob", Some(Perm::Write));
    assert_eq!(fx.history(&bob(), "f").unwrap(), std::slice::from_ref(&v3));
    assert_eq!(fx.history(&alice(), "f").unwrap(), [v1, v2, v3]);
}

#[test]
fn purge_keeps_only_the_head() {
    // contract: C14, S7
    let mut fx = Fixture::new();
    let (file, _) = fx.file("f", b"one");
    let v2 = fx.commit(&alice(), file.id, "r2", None, false, &[]).unwrap();
    fx.call(
        &alice(),
        Request::Purge {
            space: SPACE.into(),
            path: path("f"),
        },
    )
    .unwrap();
    assert_eq!(fx.history(&alice(), "f").unwrap(), [v2]);
}

#[test]
fn a_commit_on_a_pruned_base_conflicts() {
    // contract: C14
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"one");
    fx.commit(&alice(), file.id, "r2", None, false, &[]).unwrap();
    let head_only = Policy {
        versions: 1,
        max_age_ms: 0,
        grace_ms: 0,
    };
    assert_eq!(fx.authority.collect(i64::MAX / 2, &head_only).unwrap().pruned, 1);
    let rebased = fx.commit(&alice(), file.id, "r3", Some(v1.id), false, &[]);
    assert_eq!(rebased, Err(ErrorCode::Conflict));
}

#[test]
fn deleting_a_space_needs_admin_and_frees_its_name() {
    // contract: S7
    let mut fx = Fixture::new();
    fx.set_acl("", "user:bob", Some(Perm::Write));
    let delete = || Request::DeleteSpace { space: SPACE.into() };
    assert_eq!(fx.call(&bob(), delete()), Err(ErrorCode::Denied));
    fx.call(&alice(), delete()).unwrap();
    assert_eq!(fx.stat(&alice(), ""), Err(ErrorCode::NotFound));
    fx.call(&bob(), Request::CreateSpace { space: SPACE.into() })
        .unwrap();
}

#[test]
fn a_commit_replayed_after_its_version_left_retention_answers_with_the_head() {
    // contract: C4a, C5
    let mut fx = Fixture::new();
    let (file, _) = fx.file("f", b"one");
    fx.commit(&alice(), file.id, "mine", None, false, &[]).unwrap();
    let newer = fx.commit(&alice(), file.id, "newer", None, false, &[]).unwrap();
    // Fixture times are about 1,000 ms; the request ids are kept for the grace.
    let head_only = Policy {
        versions: 1,
        max_age_ms: 0,
        grace_ms: 1_000_000,
    };
    assert_eq!(fx.authority.collect(10_000, &head_only).unwrap().pruned, 2);
    let replayed = fx.commit(&alice(), file.id, "mine", None, false, &[]);
    assert_eq!(replayed, Ok(newer.clone()), "the replay commits nothing");
    assert_eq!(fx.history(&alice(), "f").unwrap(), [newer]);
}

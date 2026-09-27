//! Examples of the permission and space rules. The sim checks them at scale.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_core::proto::{ErrorCode, Perm, Request, Response};
use support::{Fixture, SPACE, alice, bob, carol, path, user};

#[test]
fn a_space_needs_a_valid_free_name_and_its_creator_becomes_admin() {
    let mut fx = Fixture::new();
    let create = |name: &str| Request::CreateSpace { space: name.into() };
    for bad in ["Team", ".x", "a/b", &"x".repeat(64)] {
        assert_eq!(fx.call(&bob(), create(bad)), Err(ErrorCode::Invalid), "{bad:?}");
    }
    assert_eq!(fx.call(&bob(), create(SPACE)), Err(ErrorCode::Exists));
    let Ok(Response::Entry { entry: root }) = fx.call(&bob(), create("b.o_b-2")) else {
        panic!()
    };
    assert!(root.path.is_root());
    let grant = Request::SetAcl {
        space: "b.o_b-2".into(),
        path: path(""),
        principal: "user:carol".into(),
        perm: None,
    };
    assert_eq!(fx.call(&bob(), grant), Ok(Response::Done));
}

#[test]
fn the_effective_perm_is_the_highest_on_the_path_or_an_ancestor() {
    // contract: S2
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "a").unwrap();
    fx.mkdir(&alice(), "a/b").unwrap();
    fx.set_acl("a", "user:carol", Some(Perm::Read));
    fx.set_acl("a/b", "group:eng", Some(Perm::Write));
    assert_eq!(fx.create(&carol(), "a/f"), Err(ErrorCode::Denied));
    assert!(fx.create(&carol(), "a/b/f").is_ok());
    assert_eq!(fx.stat(&user("dave", &[]), "a/b"), Err(ErrorCode::Denied));
}

#[test]
fn without_read_a_path_is_denied_whether_or_not_it_exists() {
    // contract: S2
    let mut fx = Fixture::new();
    fx.create(&alice(), "exists").unwrap();
    assert_eq!(fx.stat(&bob(), "exists"), Err(ErrorCode::Denied));
    assert_eq!(fx.stat(&bob(), "missing"), Err(ErrorCode::Denied));
    let stat = Request::Stat {
        space: "nowhere".into(),
        path: path(""),
    };
    assert_eq!(fx.call(&bob(), stat), Err(ErrorCode::NotFound));
}

#[test]
fn revocation_stops_new_grants_at_once() {
    // contract: S4
    let mut fx = Fixture::new();
    let (_, version) = fx.file("f", b"x");
    fx.set_acl("", "group:eng", Some(Perm::Read));
    assert!(fx.grant(&carol(), "f", version.id, &[0]).is_ok());
    fx.set_acl("", "group:eng", None);
    assert_eq!(fx.grant(&carol(), "f", version.id, &[0]), Err(ErrorCode::Denied));
}

#[test]
fn uploads_need_write_somewhere_in_the_space() {
    // contract: S6
    let mut fx = Fixture::new();
    fx.set_acl("", "user:bob", Some(Perm::Read));
    assert_eq!(fx.upload(&bob(), b"x"), Err(ErrorCode::Denied));
    fx.set_acl("deep/down", "user:bob", Some(Perm::Write));
    assert!(fx.upload(&bob(), b"x").is_ok());
}

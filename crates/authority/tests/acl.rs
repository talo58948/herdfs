//! Permissions, spaces, identity and durability.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_authority::Authority;
use herdfs_core::{
    ChunkId,
    proto::{ErrorCode, Perm, Request, Response},
};
use support::{Fixture, SPACE, alice, bob, carol, path, user};

#[test]
fn the_effective_perm_is_the_highest_on_the_path_or_an_ancestor() {
    // contract: S2
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "a").unwrap();
    fx.mkdir(&alice(), "a/b").unwrap();
    fx.set_acl(&alice(), "a", "user:carol", Some(Perm::Read)).unwrap();
    fx.set_acl(&alice(), "a/b", "group:eng", Some(Perm::Write))
        .unwrap();
    assert!(fx.stat(&carol(), "a").is_ok());
    assert_eq!(fx.create(&carol(), "a/f"), Err(ErrorCode::Denied));
    fx.create(&carol(), "a/b/f").unwrap();
    assert_eq!(fx.stat(&carol(), ""), Err(ErrorCode::Denied));
    // Group rows reach only members.
    assert_eq!(fx.stat(&user("dave", &[]), "a/b"), Err(ErrorCode::Denied));
    assert!(fx.stat(&user("dave", &["eng"]), "a/b").is_ok());
}

#[test]
fn without_read_a_path_is_denied_whether_or_not_it_exists() {
    // contract: S2
    let mut fx = Fixture::new();
    fx.file("exists", b"x");
    for at in ["exists", "missing", "missing/deeper"] {
        assert_eq!(fx.stat(&bob(), at), Err(ErrorCode::Denied), "{at}");
        assert_eq!(fx.history(&bob(), at), Err(ErrorCode::Denied), "{at}");
    }
    let stat = Request::Stat {
        space: "nowhere".into(),
        path: path(""),
    };
    assert_eq!(fx.call(&bob(), stat), Err(ErrorCode::NotFound));
}

#[test]
fn mutations_need_write_on_every_path_they_touch() {
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "open").unwrap();
    let (file, version) = fx.file("f", b"x");
    fx.set_acl(&alice(), "", "user:bob", Some(Perm::Read)).unwrap();
    fx.set_acl(&alice(), "open", "user:bob", Some(Perm::Write))
        .unwrap();
    assert_eq!(fx.create(&bob(), "g"), Err(ErrorCode::Denied));
    assert_eq!(fx.remove(&bob(), "f"), Err(ErrorCode::Denied));
    assert_eq!(fx.rename(&bob(), "f", "open/f"), Err(ErrorCode::Denied));
    assert_eq!(
        fx.commit(&bob(), file.id, "r", Some(version.id), false, &[]),
        Err(ErrorCode::Denied)
    );
    let restore = Request::Restore {
        space: SPACE.into(),
        path: path("f"),
        version: version.id,
    };
    assert_eq!(fx.call(&bob(), restore), Err(ErrorCode::Denied));
    let set_attr = Request::SetAttr {
        space: SPACE.into(),
        path: path("f"),
        mode: Some(0),
        mtime_ms: None,
    };
    assert_eq!(fx.call(&bob(), set_attr), Err(ErrorCode::Denied));
    fx.create(&bob(), "open/g").unwrap();
    fx.rename(&bob(), "open/g", "open/h").unwrap();
}

#[test]
fn revocation_stops_new_grants_at_once() {
    // contract: S4
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "d").unwrap();
    let (_, version) = fx.file("d/f", b"x");
    fx.set_acl(&alice(), "d", "group:eng", Some(Perm::Read)).unwrap();
    assert!(fx.grant(&carol(), "d/f", version.id, &[0]).is_ok());
    fx.set_acl(&alice(), "d", "group:eng", None).unwrap();
    assert_eq!(
        fx.grant(&carol(), "d/f", version.id, &[0]),
        Err(ErrorCode::Denied)
    );
}

#[test]
fn set_acl_needs_admin_on_the_path_and_a_real_principal() {
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "d").unwrap();
    assert_eq!(
        fx.set_acl(&bob(), "d", "user:bob", Some(Perm::Admin)),
        Err(ErrorCode::Denied)
    );
    fx.set_acl(&alice(), "d", "user:bob", Some(Perm::Admin)).unwrap();
    fx.set_acl(&bob(), "d", "user:carol", Some(Perm::Write)).unwrap();
    assert_eq!(
        fx.set_acl(&bob(), "", "user:carol", Some(Perm::Read)),
        Err(ErrorCode::Denied)
    );
    for bad in ["bob", "user:", "group:", "role:x"] {
        assert_eq!(
            fx.set_acl(&alice(), "d", bad, Some(Perm::Read)),
            Err(ErrorCode::Invalid),
            "{bad}"
        );
    }
    // A later row replaces the earlier one for the same principal and path.
    fx.set_acl(&alice(), "d", "user:bob", Some(Perm::Read)).unwrap();
    assert_eq!(
        fx.set_acl(&bob(), "d", "user:carol", None),
        Err(ErrorCode::Denied)
    );
}

#[test]
fn uploads_need_write_somewhere_in_the_space() {
    // contract: S6
    let mut fx = Fixture::new();
    fx.set_acl(&alice(), "", "user:bob", Some(Perm::Read)).unwrap();
    assert_eq!(fx.upload(&bob(), b"x"), Err(ErrorCode::Denied));
    fx.set_acl(&alice(), "deep/down", "user:bob", Some(Perm::Write))
        .unwrap();
    assert!(fx.upload(&bob(), b"x").is_ok());
    let missing = fx
        .authority
        .record_upload(&bob(), "nowhere", ChunkId::of(b"x"), 0);
    assert_eq!(missing.map_err(|error| error.code), Err(ErrorCode::NotFound));
}

#[test]
fn spaces_have_valid_unique_names_and_their_creator_is_admin() {
    let mut fx = Fixture::new();
    let create = |name: &str| Request::CreateSpace { space: name.into() };
    for bad in ["", "Team", "-x", ".x", "a/b", "a b", &"x".repeat(64)] {
        assert_eq!(fx.call(&bob(), create(bad)), Err(ErrorCode::Invalid), "{bad:?}");
    }
    assert_eq!(fx.call(&bob(), create(SPACE)), Err(ErrorCode::Exists));
    for good in ["0", "b.o_b-2", &"x".repeat(63)] {
        assert!(
            matches!(fx.call(&bob(), create(good)), Ok(Response::Entry { .. })),
            "{good:?}"
        );
    }
    let acl = Request::SetAcl {
        space: "0".into(),
        path: path(""),
        principal: "user:carol".into(),
        perm: None,
    };
    assert_eq!(fx.call(&bob(), acl.clone()), Ok(Response::Done));
    assert_eq!(fx.call(&alice(), acl), Err(ErrorCode::Denied));
}

#[test]
fn whoami_returns_the_principal_and_groups() {
    let mut fx = Fixture::new();
    let me = Response::Me {
        principal: "user:carol".into(),
        groups: vec!["eng".into()],
    };
    assert_eq!(fx.call(&carol(), Request::Whoami), Ok(me));
}

#[test]
fn state_survives_reopening_the_database() {
    // contract: C4b
    let mut fx = Fixture::new();
    let (_, version) = fx.file("f", b"x");
    fx.set_acl(&alice(), "", "user:bob", Some(Perm::Read)).unwrap();
    fx.reopen();
    assert_eq!(fx.history(&bob(), "f").unwrap(), [version]);
    let dir = tempfile::tempdir().unwrap();
    assert!(Authority::open(&dir.path().join("missing/dir/db")).is_err());
}

//! Database migrations preserve authority state.
#![allow(clippy::unwrap_used, reason = "test code")]

use herdfs_authority::{Authority, Outcome, Principal};
use herdfs_core::{
    SpacePath,
    proto::{Perm, Request, Response},
};

#[test]
fn a_v9_database_gains_local_tokens_without_losing_data() {
    // contract: C4b, S5
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let alice = Principal {
        user: "user:alice".into(),
        groups: vec![],
    };
    let mut authority = Authority::open(&db).unwrap();
    authority
        .handle(&alice, Request::CreateSpace { space: "kept".into() }, 1)
        .unwrap();
    drop(authority);

    let old = rusqlite::Connection::open(&db).unwrap();
    old.execute_batch("DROP TABLE local_tokens; DROP TABLE acl_history; PRAGMA user_version = 9;")
        .unwrap();
    drop(old);

    let mut authority = Authority::open(&db).unwrap();
    let Outcome::Reply(Response::Spaces { spaces }) =
        authority.handle(&alice, Request::Spaces, 2).unwrap()
    else {
        panic!("spaces returned the wrong response")
    };
    assert_eq!(spaces.len(), 1);
    assert_eq!(spaces[0].space, "kept");

    let migrated = rusqlite::Connection::open(&db).unwrap();
    let version: i64 = migrated
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 11);
    assert!(migrated.prepare("SELECT id FROM local_tokens").is_ok());
}

#[test]
fn a_v10_database_keeps_acl_access_when_history_is_added() {
    // contract: C4b, S8
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let alice = Principal {
        user: "user:alice".into(),
        groups: vec![],
    };
    let bob = Principal {
        user: "user:bob".into(),
        groups: vec![],
    };
    let mut authority = Authority::open(&db).unwrap();
    authority
        .handle(&alice, Request::CreateSpace { space: "kept".into() }, 1)
        .unwrap();
    authority
        .handle(
            &alice,
            Request::SetAcl {
                space: "kept".into(),
                path: SpacePath::root(),
                principal: bob.user.clone(),
                perm: Some(Perm::Read),
            },
            2,
        )
        .unwrap();
    drop(authority);

    let old = rusqlite::Connection::open(&db).unwrap();
    old.execute_batch("DROP TABLE acl_history; PRAGMA user_version = 10;")
        .unwrap();
    drop(old);

    let mut authority = Authority::open(&db).unwrap();
    let Outcome::Reply(Response::Spaces { spaces }) =
        authority.handle(&bob, Request::Spaces, 3).unwrap()
    else {
        panic!("spaces returned the wrong response")
    };
    assert_eq!(spaces[0].root_perm, Some(Perm::Read));
    drop(authority);

    let migrated = rusqlite::Connection::open(&db).unwrap();
    let version: i64 = migrated
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let intervals: i64 = migrated
        .query_row("SELECT count(*) FROM acl_history", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 11);
    assert_eq!(intervals, 2);
}

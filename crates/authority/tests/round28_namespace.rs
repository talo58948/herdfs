//! Red team, round 28: parent-id namespace validation and refusal ordering.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_core::proto::{EntryId, ErrorCode, Request, Response};
use support::{Fixture, SPACE, alice, bob};

/// An entry id is enumerable, but its inaccessible path is not. A name that fits below a shallow
/// parent must get the same refusal below an inaccessible parent whose hidden path is near the
/// path limit.
// contract: S3
#[test]
#[ignore = "ROUND28: child-path validation leaks an inaccessible parent path's length"]
fn an_inaccessible_parent_id_reveals_no_hidden_path_length() {
    let mut fx = Fixture::new();
    let root = fx.stat(&alice(), "").unwrap();
    let mut deep = "secret".to_owned();
    let mut parent = fx.mkdir(&alice(), &deep).unwrap();
    for _ in 0..15 {
        deep.push('/');
        deep.push_str(&"d".repeat(255));
        parent = fx.mkdir(&alice(), &deep).unwrap();
    }
    let name = "n".repeat(255);
    let create = |parent| Request::Create {
        space: SPACE.into(),
        parent,
        name: name.clone(),
        mode: 0o644,
    };

    assert_eq!(
        fx.call(&bob(), create(root.id)),
        Err(ErrorCode::Denied),
        "control below a shallow inaccessible parent"
    );
    assert_eq!(
        fx.call(&bob(), create(parent.id)),
        Err(ErrorCode::Denied),
        "the refusal must not reveal that the hidden parent is near the path limit"
    );
}

fn create(parent: EntryId, name: &str) -> Request {
    Request::Create {
        space: SPACE.into(),
        parent,
        name: name.into(),
        mode: 0o644,
    }
}

/// Parent-id namespace calls reject every non-name, accept a 255-byte Unicode name, and keep
/// canonically distinct Unicode names distinct.
// contract: C7
#[test]
fn names_are_one_valid_utf8_component() {
    let mut fx = Fixture::new();
    let root = fx.stat(&alice(), "").unwrap();
    let long = "x".repeat(256);
    for name in ["", ".", "..", "/", "a/b", "a\0b", long.as_str()] {
        assert_eq!(
            fx.call(&alice(), create(root.id, name)),
            Err(ErrorCode::Invalid),
            "{name:?}"
        );
    }

    let max_unicode = format!("{}x", "é".repeat(127));
    assert_eq!(max_unicode.len(), 255);
    assert!(fx.call(&alice(), create(root.id, &max_unicode)).is_ok());
    assert!(fx.call(&alice(), create(root.id, "é")).is_ok());
    assert!(fx.call(&alice(), create(root.id, "e\u{301}")).is_ok());
}

/// File, symlink and removed parents are never followed, parent ids are scoped to their space,
/// and a caller without access is denied before the authority reveals the parent's kind.
// contract: C7, S3
#[test]
fn parent_ids_are_scoped_live_directories() {
    let mut fx = Fixture::new();
    let root = fx.stat(&alice(), "").unwrap();
    let file = fx.create(&alice(), "file").unwrap();
    let Response::Entry { entry: link } = fx
        .call(
            &alice(),
            Request::Symlink {
                space: SPACE.into(),
                parent: root.id,
                name: "link".into(),
                target: "file".into(),
            },
        )
        .unwrap()
    else {
        panic!("symlink answers with an entry")
    };
    let removed = fx.mkdir(&alice(), "removed").unwrap();
    fx.remove(&alice(), "removed").unwrap();

    for parent in [file.id, link.id, removed.id] {
        assert_eq!(
            fx.call(&alice(), create(parent, "child")),
            Err(ErrorCode::NotFound)
        );
    }
    for parent in [file.id, link.id] {
        assert_eq!(fx.call(&bob(), create(parent, "child")), Err(ErrorCode::Denied));
    }

    let Response::Entry { entry: other_root } = fx
        .call(
            &alice(),
            Request::CreateSpace {
                space: "other".into(),
            },
        )
        .unwrap()
    else {
        panic!("create space answers with its root")
    };
    assert_eq!(
        fx.call(&alice(), create(other_root.id, "crossed")),
        Err(ErrorCode::NotFound)
    );

    let Response::Entry { entry: fork_root } = fx
        .call(
            &alice(),
            Request::Fork {
                space: SPACE.into(),
                name: "copy".into(),
            },
        )
        .unwrap()
    else {
        panic!("fork answers with its root")
    };
    let create_in_copy = |parent| Request::Create {
        space: "copy".into(),
        parent,
        name: "new".into(),
        mode: 0o644,
    };
    assert_eq!(
        fx.call(&alice(), create_in_copy(root.id)),
        Err(ErrorCode::NotFound)
    );
    assert!(fx.call(&alice(), create_in_copy(fork_root.id)).is_ok());
}

/// Replaying successful namespace requests neither repeats a rename nor affects another entry.
// contract: C7
#[test]
fn namespace_replays_fail_without_a_second_mutation() {
    let mut fx = Fixture::new();
    fx.create(&alice(), "once").unwrap();
    assert_eq!(fx.create(&alice(), "once"), Err(ErrorCode::Exists));
    fx.rename(&alice(), "once", "moved").unwrap();
    assert_eq!(fx.rename(&alice(), "once", "moved"), Err(ErrorCode::NotFound));
    fx.remove(&alice(), "moved").unwrap();
    assert_eq!(fx.remove(&alice(), "moved"), Err(ErrorCode::NotFound));
    assert_eq!(fx.stat(&alice(), "moved"), Err(ErrorCode::NotFound));
}

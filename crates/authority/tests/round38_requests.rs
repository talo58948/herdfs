//! Red team, round 38: removed parent ids after their paths are reused.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_core::proto::{ErrorCode, Request, Response};
use support::{Fixture, SPACE, alice};

/// Recreating a removed directory at the same path never makes its old id resolve again for any
/// id-addressed read or parent mutation; the replacement id remains a passing control.
// contract: C7
#[test]
fn a_recreated_directory_does_not_revive_its_removed_id() {
    for trial in 0..3 {
        let mut fx = Fixture::new();
        let root = fx.stat(&alice(), "").unwrap();
        let old = fx.mkdir(&alice(), "d").unwrap();
        fx.remove(&alice(), "d").unwrap();
        let replacement = fx.mkdir(&alice(), "d").unwrap();
        assert_ne!(old.id, replacement.id);

        let id_calls = [
            Request::Locate {
                space: SPACE.into(),
                entry: old.id,
            },
            Request::List {
                space: SPACE.into(),
                entry: old.id,
            },
            Request::Open {
                space: SPACE.into(),
                entry: old.id,
            },
            Request::SetAttr {
                space: SPACE.into(),
                entry: old.id,
                mode: Some(0o700),
                mtime_ms: Some(trial),
            },
        ];
        for request in id_calls {
            assert_eq!(fx.call(&alice(), request), Err(ErrorCode::NotFound));
        }

        let parent_calls = [
            Request::Lookup {
                space: SPACE.into(),
                parent: old.id,
                name: "child".into(),
            },
            Request::Create {
                space: SPACE.into(),
                parent: old.id,
                name: "child".into(),
                mode: 0o600,
            },
            Request::Mkdir {
                space: SPACE.into(),
                parent: old.id,
                name: "child".into(),
                mode: 0o700,
            },
            Request::Symlink {
                space: SPACE.into(),
                parent: old.id,
                name: "child".into(),
                target: "target".into(),
            },
            Request::Remove {
                space: SPACE.into(),
                parent: old.id,
                name: "child".into(),
            },
            Request::Rename {
                space: SPACE.into(),
                from_parent: old.id,
                from_name: "child".into(),
                to_parent: root.id,
                to_name: format!("moved-{trial}"),
            },
            Request::Rename {
                space: SPACE.into(),
                from_parent: root.id,
                from_name: "missing".into(),
                to_parent: old.id,
                to_name: "child".into(),
            },
        ];
        for request in parent_calls {
            assert_eq!(fx.call(&alice(), request), Err(ErrorCode::NotFound));
        }

        let control = fx
            .call(
                &alice(),
                Request::Create {
                    space: SPACE.into(),
                    parent: replacement.id,
                    name: "child".into(),
                    mode: 0o600,
                },
            )
            .unwrap();
        assert!(matches!(control, Response::Entry { .. }));
    }
}

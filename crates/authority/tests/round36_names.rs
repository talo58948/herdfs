//! Red team, round 36: name validation across every parent-id operation.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_core::proto::{ErrorCode, Request, Response};
use support::{Fixture, SPACE, alice};

/// Every parent-id operation accepts exactly one valid name component; invalid names never act on
/// a nearby valid entry, while a maximum-size name remains usable.
// contract: C7
#[test]
fn all_parent_operations_reject_non_names_without_side_effects() {
    let invalid = ["", ".", "..", "/", "a/b", "a\0b"];
    for trial in 0..3 {
        let mut fx = Fixture::new();
        let root = fx.stat(&alice(), "").unwrap();
        let file = fx.create(&alice(), "file").unwrap();
        for name in invalid {
            let calls = [
                Request::Lookup {
                    space: SPACE.into(),
                    parent: root.id,
                    name: name.into(),
                },
                Request::Create {
                    space: SPACE.into(),
                    parent: root.id,
                    name: name.into(),
                    mode: 0o600,
                },
                Request::Mkdir {
                    space: SPACE.into(),
                    parent: root.id,
                    name: name.into(),
                    mode: 0o700,
                },
                Request::Symlink {
                    space: SPACE.into(),
                    parent: root.id,
                    name: name.into(),
                    target: "target".into(),
                },
                Request::Remove {
                    space: SPACE.into(),
                    parent: root.id,
                    name: name.into(),
                },
                Request::Rename {
                    space: SPACE.into(),
                    from_parent: root.id,
                    from_name: "file".into(),
                    to_parent: root.id,
                    to_name: name.into(),
                },
            ];
            for call in calls {
                assert_eq!(
                    fx.call(&alice(), call),
                    Err(ErrorCode::Invalid),
                    "trial {trial}, {name:?}"
                );
                assert_eq!(fx.stat(&alice(), "file").unwrap().id, file.id);
            }
        }

        let max = "x".repeat(255);
        let made = fx
            .call(
                &alice(),
                Request::Create {
                    space: SPACE.into(),
                    parent: root.id,
                    name: max.clone(),
                    mode: 0o600,
                },
            )
            .unwrap();
        assert!(matches!(made, Response::Entry { .. }));
        let too_long = Request::Rename {
            space: SPACE.into(),
            from_parent: root.id,
            from_name: "file".into(),
            to_parent: root.id,
            to_name: format!("{max}x"),
        };
        assert_eq!(fx.call(&alice(), too_long), Err(ErrorCode::Invalid));
    }
}

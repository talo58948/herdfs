//! Red team, round 36: the remaining id-addressed namespace and history boundaries.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::Outcome;
use herdfs_core::proto::{Entry, EntryId, Error, ErrorCode, Perm, Request, Response};
use support::{Fixture, SPACE, alice, bob, path};

fn error(fx: &mut Fixture, request: Request) -> Error {
    match fx.authority.handle(&bob(), request, 50_000) {
        Err(error) => error,
        Ok(Outcome::Reply(reply)) => panic!("request unexpectedly answered {reply:?}"),
        Ok(Outcome::Sign { chunks }) => panic!("request unexpectedly signed {chunks:?}"),
    }
}

fn renamed(reply: Response) -> Entry {
    let Response::Renamed { entry, .. } = reply else {
        panic!("expected a renamed entry, got {reply:?}")
    };
    entry
}

/// Both parent ids of a rename are scoped to the named space. Fork ids, removed ids, and a
/// non-directory id must fail without moving either the source or its passing control.
// contract: C7
#[test]
fn rename_rejects_foreign_removed_and_non_directory_parent_ids() {
    for trial in 0..3 {
        let mut fx = Fixture::new();
        let root = fx.stat(&alice(), "").unwrap();
        let file = fx.create(&alice(), "file").unwrap();
        let removed = fx.mkdir(&alice(), "removed").unwrap();
        fx.remove(&alice(), "removed").unwrap();
        let Response::Entry { entry: copy_root } = fx
            .call(
                &alice(),
                Request::Fork {
                    space: SPACE.into(),
                    name: format!("copy-{trial}"),
                },
            )
            .unwrap()
        else {
            panic!("fork answers with its root")
        };
        let rename = |from_parent, to_parent, to_name: &str| Request::Rename {
            space: SPACE.into(),
            from_parent,
            from_name: "file".into(),
            to_parent,
            to_name: to_name.into(),
        };

        for (from, to) in [
            (copy_root.id, root.id),
            (root.id, copy_root.id),
            (removed.id, root.id),
            (root.id, removed.id),
            (file.id, root.id),
            (root.id, file.id),
        ] {
            assert_eq!(
                fx.call(&alice(), rename(from, to, "moved")),
                Err(ErrorCode::NotFound)
            );
            assert_eq!(fx.stat(&alice(), "file").unwrap().id, file.id);
            assert_eq!(fx.stat(&alice(), "moved"), Err(ErrorCode::NotFound));
        }

        assert_eq!(
            renamed(fx.call(&alice(), rename(root.id, root.id, "ok")).unwrap()).id,
            file.id
        );
    }
}

/// A denied parent path is checked before the authority reveals whether it is a file, whether a
/// child exists, or whether joining the supplied name would exceed the path limit.
// contract: S2, S3
#[test]
fn every_parent_operation_hides_inaccessible_parent_properties() {
    for trial in 0..3 {
        let mut fx = Fixture::new();
        let file = fx.create(&alice(), "secret-file").unwrap();
        let mut deep = format!("secret-{trial}");
        let mut dir = fx.mkdir(&alice(), &deep).unwrap();
        for _ in 0..15 {
            deep.push('/');
            deep.push_str(&"d".repeat(255));
            dir = fx.mkdir(&alice(), &deep).unwrap();
        }
        fx.create(&alice(), &format!("{deep}/present")).unwrap();
        let long = "n".repeat(255);
        let calls = [
            (
                Request::Lookup {
                    space: SPACE.into(),
                    parent: dir.id,
                    name: long.clone(),
                },
                "denied: needs read",
            ),
            (
                Request::Create {
                    space: SPACE.into(),
                    parent: dir.id,
                    name: long.clone(),
                    mode: 0o600,
                },
                "denied: needs write",
            ),
            (
                Request::Mkdir {
                    space: SPACE.into(),
                    parent: dir.id,
                    name: long.clone(),
                    mode: 0o700,
                },
                "denied: needs write",
            ),
            (
                Request::Symlink {
                    space: SPACE.into(),
                    parent: dir.id,
                    name: long.clone(),
                    target: "target".into(),
                },
                "denied: needs write",
            ),
            (
                Request::Remove {
                    space: SPACE.into(),
                    parent: dir.id,
                    name: "present".into(),
                },
                "denied: needs write",
            ),
            (
                Request::Lookup {
                    space: SPACE.into(),
                    parent: file.id,
                    name: "child".into(),
                },
                "denied: needs read",
            ),
        ];
        for (call, message) in calls {
            let refused = error(&mut fx, call);
            assert_eq!(
                (refused.code, refused.message.as_str()),
                (ErrorCode::Denied, message)
            );
        }
        fx.set_acl("secret-file", "user:bob", Some(Perm::Write));
        for call in [
            Request::Rename {
                space: SPACE.into(),
                from_parent: fx.stat(&alice(), "").unwrap().id,
                from_name: "secret-file".into(),
                to_parent: dir.id,
                to_name: long.clone(),
            },
            Request::Rename {
                space: SPACE.into(),
                from_parent: dir.id,
                from_name: "present".into(),
                to_parent: fx.stat(&alice(), "").unwrap().id,
                to_name: "shown".into(),
            },
        ] {
            let refused = error(&mut fx, call);
            assert_eq!(
                (refused.code, refused.message.as_str()),
                (ErrorCode::Denied, "denied: needs write")
            );
        }
    }
}

/// History is disclosed by the entry's current path: a move away revokes id-addressed reads, and
/// a move into a newly granted path shows the head but no older version from before that grant.
// contract: C7, S2, S8
#[test]
fn id_reads_and_history_use_the_entries_current_path_after_moves() {
    for trial in 0..3 {
        let mut fx = Fixture::new();
        fx.mkdir(&alice(), "old").unwrap();
        fx.mkdir(&alice(), "new").unwrap();
        fx.set_acl("old", "user:bob", Some(Perm::Read));
        let (file, first) = fx.file("old/f", b"first");
        let second = fx
            .commit(&alice(), file.id, &format!("second-{trial}"), None, false, &[])
            .unwrap();
        fx.set_acl("new", "user:bob", Some(Perm::Read));
        fx.rename(&alice(), "old/f", "new/f").unwrap();

        let locate = Request::Locate {
            space: SPACE.into(),
            entry: file.id,
        };
        let Response::Entry { entry } = fx.call(&bob(), locate).unwrap() else {
            panic!("locate answers with the moved entry")
        };
        assert_eq!(entry.path, path("new/f"));
        assert_eq!(
            fx.history(&bob(), "new/f").unwrap(),
            std::slice::from_ref(&second)
        );
        assert_eq!(
            fx.grant(&bob(), file.id, first.id, &[0]),
            Err(ErrorCode::NotFound)
        );
        assert_eq!(fx.grant(&bob(), file.id, second.id, &[]), Ok(Vec::new()));

        fx.rename(&alice(), "new/f", "old/f").unwrap();
        fx.set_acl("old", "user:bob", None);
        for call in [
            Request::Locate {
                space: SPACE.into(),
                entry: file.id,
            },
            Request::Open {
                space: SPACE.into(),
                entry: file.id,
            },
        ] {
            assert_eq!(fx.call(&bob(), call), Err(ErrorCode::Denied));
        }
    }
}

/// An unknown id remains distinguishable from a known but inaccessible parent, as ids are not
/// secret; it cannot be mistaken for a permission-dependent refusal.
// contract: C7 (docs/architecture.md: entry ids are not secret)
#[test]
fn unknown_parent_control_is_not_a_permission_refusal() {
    let mut fx = Fixture::new();
    let request = Request::Lookup {
        space: SPACE.into(),
        parent: EntryId(999_999),
        name: "child".into(),
    };
    assert_eq!(fx.call(&bob(), request), Err(ErrorCode::NotFound));
}

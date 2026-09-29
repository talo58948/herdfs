//! Red team, round 34: id-addressed request scope and refusal ordering. These probes held and
//! remain as regression tests.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::Outcome;
use herdfs_core::proto::{Entry, EntryId, Error, ErrorCode, Perm, Request, Response};
use support::{Fixture, SPACE, alice, bob, path};

fn entry(reply: Response) -> Entry {
    let Response::Entry { entry } = reply else {
        panic!("expected an entry, got {reply:?}")
    };
    entry
}

fn wrong_space(request: Request) -> Request {
    match request {
        Request::Locate { entry, .. } => Request::Locate {
            space: "copy".into(),
            entry,
        },
        Request::Lookup { parent, name, .. } => Request::Lookup {
            space: "copy".into(),
            parent,
            name,
        },
        Request::List { entry, .. } => Request::List {
            space: "copy".into(),
            entry,
        },
        Request::Open { entry, .. } => Request::Open {
            space: "copy".into(),
            entry,
        },
        Request::Create {
            parent, name, mode, ..
        } => Request::Create {
            space: "copy".into(),
            parent,
            name,
            mode,
        },
        Request::Mkdir {
            parent, name, mode, ..
        } => Request::Mkdir {
            space: "copy".into(),
            parent,
            name,
            mode,
        },
        Request::Symlink {
            parent, name, target, ..
        } => Request::Symlink {
            space: "copy".into(),
            parent,
            name,
            target,
        },
        Request::Remove { parent, name, .. } => Request::Remove {
            space: "copy".into(),
            parent,
            name,
        },
        Request::SetAttr {
            entry,
            mode,
            mtime_ms,
            ..
        } => Request::SetAttr {
            space: "copy".into(),
            entry,
            mode,
            mtime_ms,
        },
        other => panic!("unsupported request {other:?}"),
    }
}

/// Every new id-addressed operation rejects an id from a fork, another space, or a removed
/// entry. Correct-space controls still resolve the same ids and names.
// contract: C7, S3
#[test]
fn ids_are_scoped_to_live_entries_in_the_named_space() {
    let mut fx = Fixture::new();
    let root = fx.stat(&alice(), "").unwrap();
    let dir = fx.mkdir(&alice(), "d").unwrap();
    let file = fx.create(&alice(), "d/f").unwrap();
    let removed = fx.create(&alice(), "gone").unwrap();
    fx.remove(&alice(), "gone").unwrap();
    fx.call(
        &alice(),
        Request::Fork {
            space: SPACE.into(),
            name: "copy".into(),
        },
    )
    .unwrap();

    let calls = [
        Request::Locate {
            space: SPACE.into(),
            entry: file.id,
        },
        Request::Lookup {
            space: SPACE.into(),
            parent: dir.id,
            name: "f".into(),
        },
        Request::List {
            space: SPACE.into(),
            entry: dir.id,
        },
        Request::Open {
            space: SPACE.into(),
            entry: file.id,
        },
        Request::Create {
            space: SPACE.into(),
            parent: root.id,
            name: "new-file".into(),
            mode: 0o600,
        },
        Request::Mkdir {
            space: SPACE.into(),
            parent: root.id,
            name: "new-dir".into(),
            mode: 0o700,
        },
        Request::Symlink {
            space: SPACE.into(),
            parent: root.id,
            name: "new-link".into(),
            target: "d/f".into(),
        },
        Request::Remove {
            space: SPACE.into(),
            parent: dir.id,
            name: "f".into(),
        },
        Request::SetAttr {
            space: SPACE.into(),
            entry: file.id,
            mode: Some(0o640),
            mtime_ms: None,
        },
    ];
    for request in calls {
        assert_eq!(fx.call(&alice(), wrong_space(request)), Err(ErrorCode::NotFound));
    }

    for id in [removed.id, EntryId(i64::MAX as u64)] {
        for request in [
            Request::Locate {
                space: SPACE.into(),
                entry: id,
            },
            Request::List {
                space: SPACE.into(),
                entry: id,
            },
            Request::Open {
                space: SPACE.into(),
                entry: id,
            },
            Request::SetAttr {
                space: SPACE.into(),
                entry: id,
                mode: Some(0o600),
                mtime_ms: None,
            },
        ] {
            assert_eq!(fx.call(&alice(), request), Err(ErrorCode::NotFound));
        }
    }

    assert!(
        fx.call(
            &alice(),
            Request::Locate {
                space: SPACE.into(),
                entry: file.id
            }
        )
        .is_ok()
    );
    assert!(
        fx.call(
            &alice(),
            Request::List {
                space: SPACE.into(),
                entry: dir.id
            }
        )
        .is_ok()
    );
    assert!(
        fx.call(
            &alice(),
            Request::Open {
                space: SPACE.into(),
                entry: file.id
            }
        )
        .is_ok()
    );
}

fn refused(fx: &mut Fixture, request: Request) -> Error {
    match fx.authority.handle(&bob(), request, 50_000) {
        Err(error) => error,
        Ok(Outcome::Reply(reply)) => panic!("request unexpectedly answered {reply:?}"),
        Ok(Outcome::Sign { chunks }) => panic!("request unexpectedly signed {chunks:?}"),
    }
}

/// Once an id resolves, inaccessible files and directories get path-free refusals before their
/// kind, child existence, or hidden path length can affect the answer.
// contract: S2, S3
#[test]
fn inaccessible_ids_reveal_no_path_kind_or_child_existence() {
    let mut fx = Fixture::new();
    let file = fx.create(&alice(), "secret-file").unwrap();
    let dir = fx.mkdir(&alice(), "secret-dir").unwrap();
    fx.create(&alice(), "secret-dir/present").unwrap();

    let read = [
        Request::List {
            space: SPACE.into(),
            entry: file.id,
        },
        Request::List {
            space: SPACE.into(),
            entry: dir.id,
        },
        Request::Open {
            space: SPACE.into(),
            entry: file.id,
        },
        Request::Open {
            space: SPACE.into(),
            entry: dir.id,
        },
        Request::Lookup {
            space: SPACE.into(),
            parent: dir.id,
            name: "present".into(),
        },
        Request::Lookup {
            space: SPACE.into(),
            parent: dir.id,
            name: "absent".into(),
        },
        Request::Lookup {
            space: SPACE.into(),
            parent: file.id,
            name: "child".into(),
        },
    ];
    let answers: Vec<_> = read
        .into_iter()
        .map(|request| refused(&mut fx, request))
        .collect();
    assert!(answers.iter().all(|error| error.code == ErrorCode::Denied));
    assert!(answers.iter().all(|error| error.message == "denied: needs read"));
    assert!(answers.iter().all(|error| !error.message.contains("secret")));

    let write = [file.id, dir.id].map(|entry| Request::SetAttr {
        space: SPACE.into(),
        entry,
        mode: Some(0o600),
        mtime_ms: None,
    });
    for request in write {
        let error = refused(&mut fx, request);
        assert_eq!(
            (error.code, error.message.as_str()),
            (ErrorCode::Denied, "denied: needs write")
        );
    }

    fx.set_acl("secret-dir", "user:bob", Some(Perm::Read));
    let shown = entry(
        fx.call(
            &bob(),
            Request::Lookup {
                space: SPACE.into(),
                parent: dir.id,
                name: "present".into(),
            },
        )
        .unwrap(),
    );
    assert_eq!(shown.path, path("secret-dir/present"));
}

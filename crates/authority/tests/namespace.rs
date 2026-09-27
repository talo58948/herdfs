//! Namespace rules: create, list, remove, rename and attributes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_core::proto::{ErrorCode, Kind, Request, Response};
use support::{Fixture, SPACE, alice, path};

#[test]
fn the_root_is_a_directory_entry() {
    let mut fx = Fixture::new();
    let root = fx.stat(&alice(), "").unwrap();
    assert_eq!(
        (root.kind, root.path.is_root(), root.head),
        (Kind::Dir, true, None)
    );
    assert_eq!(fx.mkdir(&alice(), ""), Err(ErrorCode::Exists));
}

#[test]
fn creation_needs_a_parent_directory_and_a_free_name() {
    let mut fx = Fixture::new();
    assert_eq!(fx.create(&alice(), "a/f"), Err(ErrorCode::NotFound));
    fx.create(&alice(), "f").unwrap();
    assert_eq!(fx.create(&alice(), "f/g"), Err(ErrorCode::NotDir));
    assert_eq!(fx.create(&alice(), "f"), Err(ErrorCode::Exists));
    assert_eq!(fx.mkdir(&alice(), "f"), Err(ErrorCode::Exists));
    let symlink = Request::Symlink {
        space: SPACE.into(),
        path: path("f"),
        target: "x".into(),
    };
    assert_eq!(fx.entry(&alice(), symlink), Err(ErrorCode::Exists));

    fx.now_ms = 5_000;
    let dir = fx.mkdir(&alice(), "a").unwrap();
    assert_eq!((dir.kind, dir.mode, dir.mtime_ms), (Kind::Dir, 0o755, 5_001));
    let file = fx.create(&alice(), "a/f").unwrap();
    assert_eq!(
        (file.kind, file.mode, file.head, file.size),
        (Kind::File, 0o644, None, 0)
    );
    assert_ne!(dir.id, file.id);
}

#[test]
fn symlinks_keep_their_target() {
    let mut fx = Fixture::new();
    let request = Request::Symlink {
        space: SPACE.into(),
        path: path("link"),
        target: "../elsewhere".into(),
    };
    let link = fx.entry(&alice(), request).unwrap();
    assert_eq!(
        (link.kind, link.target.as_deref()),
        (Kind::Symlink, Some("../elsewhere"))
    );
    assert_eq!(fx.stat(&alice(), "link").unwrap(), link);
}

#[test]
fn list_shows_direct_children_of_a_directory() {
    let mut fx = Fixture::new();
    for dir in ["a", "a/b", "ab"] {
        fx.mkdir(&alice(), dir).unwrap();
    }
    fx.create(&alice(), "a/b/f").unwrap();
    fx.create(&alice(), "a/g").unwrap();
    assert_eq!(fx.list(&alice(), "").unwrap(), ["a", "ab"]);
    assert_eq!(fx.list(&alice(), "a").unwrap(), ["a/b", "a/g"]);
    assert_eq!(fx.list(&alice(), "a/g"), Err(ErrorCode::NotDir));
    assert_eq!(fx.list(&alice(), "zz"), Err(ErrorCode::NotFound));
}

#[test]
fn remove_takes_files_and_empty_directories_but_not_the_root() {
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "a").unwrap();
    fx.file("a/f", b"bytes");
    assert_eq!(fx.remove(&alice(), "a"), Err(ErrorCode::NotEmpty));
    assert_eq!(fx.remove(&alice(), ""), Err(ErrorCode::Invalid));
    assert_eq!(fx.remove(&alice(), "a/zz"), Err(ErrorCode::NotFound));
    fx.remove(&alice(), "a/f").unwrap();
    assert_eq!(fx.stat(&alice(), "a/f"), Err(ErrorCode::NotFound));
    fx.remove(&alice(), "a").unwrap();
    assert_eq!(fx.list(&alice(), "").unwrap(), Vec::<String>::new());
}

#[test]
fn rename_moves_a_directory_with_its_subtree() {
    // contract: C7
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "a").unwrap();
    let inner = fx.mkdir(&alice(), "a/b").unwrap();
    let (file, version) = fx.file("a/b/f", b"bytes");
    fx.mkdir(&alice(), "x").unwrap();
    fx.rename(&alice(), "a", "x/moved").unwrap();

    assert_eq!(fx.stat(&alice(), "x/moved/b").unwrap().id, inner.id);
    let moved = fx.stat(&alice(), "x/moved/b/f").unwrap();
    assert_eq!((moved.id, moved.head), (file.id, Some(version.id)));
    for old in ["a", "a/b", "a/b/f"] {
        assert_eq!(fx.stat(&alice(), old), Err(ErrorCode::NotFound), "{old}");
    }
    assert_eq!(fx.history(&alice(), "x/moved/b/f").unwrap(), [version]);
}

#[test]
fn rename_replaces_a_file_or_an_empty_directory_of_the_same_kind() {
    // contract: C7
    let mut fx = Fixture::new();
    let (source, _) = fx.file("f", b"one");
    fx.file("g", b"two");
    fx.rename(&alice(), "f", "g").unwrap();
    assert_eq!(fx.stat(&alice(), "g").unwrap().id, source.id);
    assert_eq!(fx.stat(&alice(), "f"), Err(ErrorCode::NotFound));

    for dir in ["d", "empty", "full", "full/x"] {
        fx.mkdir(&alice(), dir).unwrap();
    }
    assert_eq!(fx.rename(&alice(), "d", "full"), Err(ErrorCode::NotEmpty));
    assert_eq!(fx.rename(&alice(), "d", "g"), Err(ErrorCode::NotDir));
    assert_eq!(fx.rename(&alice(), "g", "empty"), Err(ErrorCode::IsDir));
    let dir = fx.stat(&alice(), "d").unwrap();
    fx.rename(&alice(), "d", "empty").unwrap();
    assert_eq!(fx.stat(&alice(), "empty").unwrap().id, dir.id);
}

#[test]
fn rename_into_itself_is_invalid_and_onto_itself_changes_nothing() {
    // contract: C7
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "a").unwrap();
    fx.mkdir(&alice(), "a/b").unwrap();
    assert_eq!(fx.rename(&alice(), "a", "a/b/c"), Err(ErrorCode::Invalid));
    assert_eq!(fx.rename(&alice(), "", "z"), Err(ErrorCode::Invalid));
    assert_eq!(fx.rename(&alice(), "a/b", "a"), Err(ErrorCode::NotEmpty));
    assert_eq!(fx.rename(&alice(), "zz", "y"), Err(ErrorCode::NotFound));
    assert_eq!(fx.rename(&alice(), "a", "no/such"), Err(ErrorCode::NotFound));
    let before = fx.stat(&alice(), "a").unwrap();
    assert_eq!(fx.rename(&alice(), "a", "a"), Ok(Response::Done));
    assert_eq!(fx.stat(&alice(), "a").unwrap(), before);
    assert_eq!(fx.list(&alice(), "a").unwrap(), ["a/b"]);
}

#[test]
fn set_attr_changes_only_the_given_fields() {
    let mut fx = Fixture::new();
    let file = fx.create(&alice(), "f").unwrap();
    let request = |mode, mtime_ms| Request::SetAttr {
        space: SPACE.into(),
        path: path("f"),
        mode,
        mtime_ms,
    };
    let changed = fx.entry(&alice(), request(Some(0o600), None)).unwrap();
    assert_eq!((changed.mode, changed.mtime_ms), (0o600, file.mtime_ms));
    let changed = fx.entry(&alice(), request(None, Some(42))).unwrap();
    assert_eq!((changed.mode, changed.mtime_ms), (0o600, 42));
    assert_eq!(fx.stat(&alice(), "f").unwrap(), changed);
    assert_eq!(
        fx.entry(
            &alice(),
            Request::SetAttr {
                space: SPACE.into(),
                path: path("zz"),
                mode: None,
                mtime_ms: None
            }
        ),
        Err(ErrorCode::NotFound)
    );
}

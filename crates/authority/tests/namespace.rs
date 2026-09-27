//! Examples of the namespace and rename rules. The sim checks them at scale.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_core::proto::ErrorCode;
use support::{Fixture, alice};

#[test]
fn creation_needs_a_directory_parent_and_a_free_name() {
    let mut fx = Fixture::new();
    assert_eq!(fx.create(&alice(), "a/f"), Err(ErrorCode::NotFound));
    fx.create(&alice(), "f").unwrap();
    assert_eq!(fx.create(&alice(), "f/g"), Err(ErrorCode::NotDir));
    assert_eq!(fx.mkdir(&alice(), "f"), Err(ErrorCode::Exists));
}

#[test]
fn remove_refuses_a_non_empty_directory_and_the_root() {
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "a").unwrap();
    fx.create(&alice(), "a/f").unwrap();
    assert_eq!(fx.remove(&alice(), "a"), Err(ErrorCode::NotEmpty));
    assert_eq!(fx.remove(&alice(), ""), Err(ErrorCode::Invalid));
}

#[test]
fn rename_moves_a_directory_with_its_subtree() {
    // contract: C7
    let mut fx = Fixture::new();
    fx.mkdir(&alice(), "a").unwrap();
    fx.mkdir(&alice(), "a/b").unwrap();
    let (file, version) = fx.file("a/b/f", b"bytes");
    fx.rename(&alice(), "a", "moved").unwrap();
    let moved = fx.stat(&alice(), "moved/b/f").unwrap();
    assert_eq!((moved.id, moved.head), (file.id, Some(version.id)));
    assert_eq!(fx.stat(&alice(), "a/b/f"), Err(ErrorCode::NotFound));
}

#[test]
fn rename_replaces_only_a_file_or_an_empty_directory_of_the_same_kind() {
    // contract: C7
    let mut fx = Fixture::new();
    let source = fx.create(&alice(), "f").unwrap();
    fx.create(&alice(), "g").unwrap();
    for dir in ["d", "empty", "full", "full/x"] {
        fx.mkdir(&alice(), dir).unwrap();
    }
    assert_eq!(fx.rename(&alice(), "d", "full"), Err(ErrorCode::NotEmpty));
    assert_eq!(fx.rename(&alice(), "d", "g"), Err(ErrorCode::NotDir));
    assert_eq!(fx.rename(&alice(), "f", "empty"), Err(ErrorCode::IsDir));
    fx.rename(&alice(), "f", "g").unwrap();
    assert_eq!(fx.stat(&alice(), "g").unwrap().id, source.id);
    assert!(fx.rename(&alice(), "d", "empty").is_ok());
}

#[test]
fn rename_into_itself_is_invalid_and_onto_itself_changes_nothing() {
    // contract: C7
    let mut fx = Fixture::new();
    let dir = fx.mkdir(&alice(), "a").unwrap();
    assert_eq!(fx.rename(&alice(), "a", "a/b"), Err(ErrorCode::Invalid));
    fx.rename(&alice(), "a", "a").unwrap();
    assert_eq!(fx.stat(&alice(), "a"), Ok(dir));
}

#[test]
fn adding_or_taking_an_entry_moves_its_directory_mtime() {
    let mut fx = Fixture::new();
    let dir = fx.mkdir(&alice(), "a").unwrap();
    assert_eq!(fx.stat(&alice(), "").unwrap().mtime_ms, dir.mtime_ms);
    let file = fx.create(&alice(), "a/f").unwrap();
    assert_eq!(fx.stat(&alice(), "a").unwrap().mtime_ms, file.mtime_ms);
    fx.rename(&alice(), "a/f", "g").unwrap();
    let (old_parent, new_parent) = (fx.stat(&alice(), "a").unwrap(), fx.stat(&alice(), "").unwrap());
    assert!(old_parent.mtime_ms > file.mtime_ms);
    assert_eq!(new_parent.mtime_ms, old_parent.mtime_ms);
    fx.remove(&alice(), "g").unwrap();
    assert!(fx.stat(&alice(), "").unwrap().mtime_ms > new_parent.mtime_ms);
}

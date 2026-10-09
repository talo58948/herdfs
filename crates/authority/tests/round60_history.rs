//! Round 60: multi-link access continuity across paths and principal kinds.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_core::proto::{Perm, VersionId};
use support::{Fixture, alice, user};

fn history_ids(fixture: &mut Fixture, path: &str) -> Vec<VersionId> {
    fixture
        .history(&user("bob", &["eng"]), path)
        .unwrap()
        .into_iter()
        .map(|version| version.id)
        .collect()
}

fn three_link_handoff(trial: usize, overlap: bool) -> bool {
    let mut fixture = Fixture::new();
    let parent = format!("parent-{trial}");
    let child = format!("{parent}/child");
    let path = format!("{child}/file");
    fixture.mkdir(&alice(), &parent).unwrap();
    fixture.mkdir(&alice(), &child).unwrap();
    let (file, before) = fixture.file(&path, b"before");

    fixture.set_acl(&child, "user:bob", Some(Perm::Read));
    let visible = fixture
        .commit(&alice(), file.id, &format!("visible-{trial}"), None, false, &[])
        .unwrap();
    if !overlap {
        fixture.set_acl(&child, "user:bob", None);
    }
    fixture.set_acl(&parent, "group:eng", Some(Perm::Read));
    if overlap {
        fixture.set_acl(&child, "user:bob", None);
    } else {
        fixture.set_acl(&parent, "group:eng", None);
    }
    fixture.set_acl(&path, "user:bob", Some(Perm::Read));
    if overlap {
        fixture.set_acl(&parent, "group:eng", None);
    }
    let head = fixture
        .commit(&alice(), file.id, &format!("head-{trial}"), None, false, &[])
        .unwrap();

    let history = history_ids(&mut fixture, &path);
    if overlap {
        history == [visible.id, head.id] && !history.contains(&before.id)
    } else {
        history == [head.id] && !history.contains(&visible.id) && !history.contains(&before.id)
    }
}

/// A chain of two overlapping handoffs keeps the first boundary across a path move and a
/// user/group/user transition; revoking before each next grant resets it as the control.
// contract: C14, S8
#[test]
fn three_link_access_handoffs_keep_the_first_history_boundary() {
    for trial in 0..3 {
        assert!(three_link_handoff(trial, true), "overlap trial {trial}");
        assert!(three_link_handoff(trial + 10, false), "gap control {trial}");
    }
}

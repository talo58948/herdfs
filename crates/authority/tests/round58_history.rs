//! Round 58: uninterrupted S8 access across ACL handoffs not covered by round 56.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_core::proto::{Perm, VersionId};
use support::{Fixture, alice, path as space_path, user};

fn ids(fixture: &mut Fixture, who: &herdfs_authority::Principal, path: &str) -> Vec<VersionId> {
    fixture
        .history(who, path)
        .unwrap()
        .into_iter()
        .map(|version| version.id)
        .collect()
}

fn descendant_handoff(trial: usize, overlap: bool) -> (bool, bool) {
    let mut fixture = Fixture::new();
    let parent = format!("p-{trial}");
    let child = format!("{parent}/c");
    let path = format!("{child}/f");
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
    fixture.set_acl(&parent, "user:bob", Some(Perm::Read));
    if overlap {
        fixture.set_acl(&child, "user:bob", None);
    }
    let head = fixture
        .commit(&alice(), file.id, &format!("head-{trial}"), None, false, &[])
        .unwrap();
    let history = ids(&mut fixture, &support::bob(), &path);
    (
        history == [visible.id, head.id],
        history == [head.id] && !history.contains(&before.id),
    )
}

/// Moving uninterrupted access from a descendant ACL row to an already-overlapping ancestor row
/// keeps the original history boundary; an actual revoke before the ancestor grant is the control.
// contract: C14, S8
#[test]
#[ignore = "ROUND58: descendant-to-ancestor ACL handoff resets readable history"]
fn descendant_to_ancestor_handoffs_keep_readable_history() {
    let mut lost = 0;
    for trial in 0..3 {
        let (kept, _) = descendant_handoff(trial, true);
        lost += usize::from(!kept);
        let (_, reset) = descendant_handoff(trial + 10, false);
        assert!(reset, "revoke and re-grant control, trial {trial}");
    }
    assert_eq!(lost, 0, "{lost}/3 overlapping handoffs lost readable history");
}

fn group_handoff(trial: usize, overlap: bool) -> (bool, bool) {
    let mut fixture = Fixture::new();
    let path = format!("group-{trial}");
    let (file, before) = fixture.file(&path, b"before");
    fixture.set_acl(&path, "group:eng", Some(Perm::Read));
    let visible = fixture
        .commit(
            &alice(),
            file.id,
            &format!("visible-group-{trial}"),
            None,
            false,
            &[],
        )
        .unwrap();
    if !overlap {
        fixture.set_acl(&path, "group:eng", None);
    }
    fixture.set_acl(&path, "user:dave", Some(Perm::Read));
    if overlap {
        fixture.set_acl(&path, "group:eng", None);
    }
    let head = fixture
        .commit(
            &alice(),
            file.id,
            &format!("head-group-{trial}"),
            None,
            false,
            &[],
        )
        .unwrap();
    let dave = user("dave", &["eng"]);
    let history = ids(&mut fixture, &dave, space_path(&path).as_str());
    (
        history == [visible.id, head.id],
        history == [head.id] && !history.contains(&before.id),
    )
}

/// Moving uninterrupted access from a group ACL row to an already-overlapping direct-user row
/// keeps the group grant's history boundary; a real revoke before the user grant is the control.
// contract: C14, S8
#[test]
#[ignore = "ROUND58: group-to-user ACL handoff resets readable history"]
fn group_to_user_handoffs_keep_readable_history() {
    let mut lost = 0;
    for trial in 0..3 {
        let (kept, _) = group_handoff(trial, true);
        lost += usize::from(!kept);
        let (_, reset) = group_handoff(trial + 10, false);
        assert!(reset, "revoke and re-grant control, trial {trial}");
    }
    assert_eq!(lost, 0, "{lost}/3 overlapping handoffs lost readable history");
}

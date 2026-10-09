//! Round 60: multi-link access continuity across paths and principal kinds.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::Principal;
use herdfs_core::proto::{Perm, Request, Response, VersionId};
use support::{Fixture, SPACE, alice, path as space_path, user};

fn history_ids(fixture: &mut Fixture, path: &str) -> Vec<VersionId> {
    fixture
        .history(&user("bob", &["eng"]), path)
        .unwrap()
        .into_iter()
        .map(|version| version.id)
        .collect()
}

fn history_ids_in(fixture: &mut Fixture, who: &Principal, space: &str, path: &str) -> Vec<VersionId> {
    let request = Request::History {
        space: space.into(),
        path: space_path(path),
    };
    let Response::History { versions } = fixture.call(who, request).unwrap() else {
        panic!("history returned another response")
    };
    versions.into_iter().map(|version| version.id).collect()
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

fn inherited_fork_history(trial: usize) -> (bool, bool) {
    let mut fixture = Fixture::new();
    let path = format!("forked-{trial}");
    let (file, before) = fixture.file(&path, b"before access");
    fixture.set_acl(&path, "user:bob", Some(Perm::Read));
    let parent_head = fixture
        .commit(&alice(), file.id, &format!("parent-{trial}"), None, false, &[])
        .unwrap();
    assert_eq!(history_ids(&mut fixture, &path), [parent_head.id]);

    let copy = format!("copy-{trial}");
    fixture
        .call(
            &alice(),
            Request::Fork {
                space: SPACE.into(),
                name: copy.clone(),
            },
        )
        .unwrap();
    let Response::Entry { entry } = fixture
        .call(
            &alice(),
            Request::Stat {
                space: copy.clone(),
                path: space_path(&path),
            },
        )
        .unwrap()
    else {
        panic!("stat returned another response")
    };
    let copied = entry.head.unwrap();
    fixture
        .call(
            &alice(),
            Request::SetAcl {
                space: copy.clone(),
                path: space_path(&path),
                principal: "user:late".into(),
                perm: Some(Perm::Read),
            },
        )
        .unwrap();
    let Response::Version { version: newer } = fixture
        .call(
            &alice(),
            Request::Commit {
                space: copy.clone(),
                entry: entry.id,
                request_id: format!("copy-head-{trial}"),
                base: None,
                guarded: false,
                chunks: vec![],
                mtime_ms: None,
            },
        )
        .unwrap()
    else {
        panic!("commit returned another response")
    };

    let inherited = history_ids_in(&mut fixture, &user("bob", &[]), &copy, &path);
    let late = history_ids_in(&mut fixture, &user("late", &[]), &copy, &path);
    (
        inherited == [copied, newer.id] && !inherited.contains(&before.id),
        late == [newer.id] && !late.contains(&copied),
    )
}

/// A grant copied by a fork starts with the copied head and keeps that first fork version visible
/// after a new head; a principal granted only after the fork is the strict-after control.
// contract: C13, C14, S8
#[test]
fn copied_access_keeps_the_forks_initial_version_visible() {
    let mut hidden = 0;
    for trial in 0..3 {
        let (visible, late_hidden) = inherited_fork_history(trial);
        hidden += usize::from(!visible);
        assert!(late_hidden, "late-grant control {trial}");
    }
    assert_eq!(hidden, 0, "{hidden}/3 inherited grants lost the fork's first version");
}

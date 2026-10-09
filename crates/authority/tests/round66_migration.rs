//! Round 66: access continuity lost by the actual schema-v10 ACL representation.
#![allow(clippy::unwrap_used, reason = "test code")]

use herdfs_authority::{Authority, Outcome, Principal};
use herdfs_core::{
    SpacePath,
    proto::{Entry, EntryId, Perm, Request, Response, Version, VersionId},
};

const SPACE: &str = "team";

fn user(name: &str) -> Principal {
    Principal {
        user: format!("user:{name}"),
        groups: vec![],
    }
}

fn call(authority: &mut Authority, who: &Principal, request: Request) -> Response {
    let Outcome::Reply(response) = authority.handle(who, request, 1).unwrap() else {
        panic!("request unexpectedly signed chunks")
    };
    response
}

fn entry(authority: &mut Authority, request: Request) -> Entry {
    let Response::Entry { entry } = call(authority, &user("alice"), request) else {
        panic!("request did not return an entry")
    };
    entry
}

fn make(authority: &mut Authority, parent: EntryId, name: &str, dir: bool) -> Entry {
    let request = if dir {
        Request::Mkdir {
            space: SPACE.into(),
            parent,
            name: name.into(),
            mode: 0o755,
        }
    } else {
        Request::Create {
            space: SPACE.into(),
            parent,
            name: name.into(),
            mode: 0o644,
        }
    };
    entry(authority, request)
}

fn commit(authority: &mut Authority, file: EntryId, request_id: String) -> Version {
    let Response::Version { version } = call(
        authority,
        &user("alice"),
        Request::Commit {
            space: SPACE.into(),
            entry: file,
            request_id,
            base: None,
            guarded: false,
            chunks: vec![],
            mtime_ms: None,
        },
    ) else {
        panic!("commit did not return a version")
    };
    version
}

fn set_acl(authority: &mut Authority, path: &str, principal: &str, perm: Option<Perm>) {
    call(
        authority,
        &user("alice"),
        Request::SetAcl {
            space: SPACE.into(),
            path: SpacePath::try_from(path).unwrap(),
            principal: principal.into(),
            perm,
        },
    );
}

fn history(authority: &mut Authority, who: &str, path: &str) -> Vec<VersionId> {
    let Response::History { versions } = call(
        authority,
        &user(who),
        Request::History {
            space: SPACE.into(),
            path: SpacePath::try_from(path).unwrap(),
        },
    ) else {
        panic!("history did not return versions")
    };
    versions.into_iter().map(|version| version.id).collect()
}

/// V10 recorded only the surviving ancestor row after an overlapping descendant-to-ancestor
/// handoff, with the same floor as a genuinely late grant. Migration must not hide history that
/// remained continuously readable; the late principal controls strict post-grant visibility.
// contract: C4b, C14, S8
#[test]
#[ignore = "ROUND66: v10 migration cannot recover descendant-to-ancestor continuity"]
fn v10_descendant_handoffs_keep_their_history_after_migration() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let mut authority = Authority::open(&db).unwrap();
    let root = entry(&mut authority, Request::CreateSpace { space: SPACE.into() });
    let mut attacks = Vec::new();
    for trial in 0..3 {
        let parent = make(&mut authority, root.id, &format!("p-{trial}"), true);
        let child = make(&mut authority, parent.id, "child", true);
        let file = make(&mut authority, child.id, "file", false);
        commit(&mut authority, file.id, format!("before-{trial}"));
        let child_path = format!("p-{trial}/child");
        let file_path = format!("{child_path}/file");
        set_acl(&mut authority, &child_path, "user:bob", Some(Perm::Read));
        let visible = commit(&mut authority, file.id, format!("visible-{trial}"));
        let parent_path = format!("p-{trial}");
        set_acl(&mut authority, &parent_path, "user:bob", Some(Perm::Read));
        set_acl(&mut authority, &child_path, "user:bob", None);
        set_acl(&mut authority, &parent_path, "user:late", Some(Perm::Read));
        attacks.push((file.id, file_path, parent_path, visible.id));
    }
    drop(authority);

    let old = rusqlite::Connection::open(&db).unwrap();
    for (_, _, parent, visible) in &attacks {
        old.execute(
            "UPDATE acl SET granted_seq = ?1
             WHERE space = ?2 AND path = ?3 AND principal = 'user:bob'",
            rusqlite::params![visible.0, SPACE, parent],
        )
        .unwrap();
    }
    old.execute_batch("DROP TABLE acl_history; PRAGMA user_version = 10;")
        .unwrap();
    drop(old);

    let mut authority = Authority::open(&db).unwrap();
    let mut lost = 0;
    for (trial, (file, path, _, visible)) in attacks.into_iter().enumerate() {
        let head = commit(&mut authority, file, format!("head-{trial}"));
        let uninterrupted = history(&mut authority, "bob", &path);
        let late = history(&mut authority, "late", &path);
        lost += usize::from(uninterrupted != [visible, head.id]);
        assert_eq!(late, [head.id], "late-grant control {trial}");
    }
    assert_eq!(lost, 0, "{lost}/3 migrated handoffs lost readable history");
}

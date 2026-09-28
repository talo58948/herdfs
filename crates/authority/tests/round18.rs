//! Red team, round 18 probes: per-commit uploads under shared and odd request ids, and history
//! across a fork and a deleted space. All held; they stay as regression tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_authority::{Outcome, Policy, Principal};
use herdfs_core::proto::{EntryId, ErrorCode, Perm, Request, Response, VersionId};
use herdfs_core::{ChunkId, ChunkRef};
use support::{Fixture, SPACE, alice, bob, path};

const GRACE_MS: i64 = 60_000;
/// Well after the fixture's clock, which starts at 1 s.
const UPLOADED_MS: i64 = 100_000;

/// Keeps every version; only upload grace matters.
fn keep_versions() -> Policy {
    Policy {
        versions: u32::MAX,
        max_age_ms: i64::MAX / 4,
        grace_ms: GRACE_MS,
    }
}

fn chunk(bytes: &[u8]) -> ChunkRef {
    ChunkRef {
        id: ChunkId::of(bytes),
        len: u32::try_from(bytes.len()).unwrap(),
    }
}

fn record(fx: &mut Fixture, who: &Principal, request_id: &str, bytes: &[u8], at_ms: i64) -> ChunkRef {
    let chunk = chunk(bytes);
    fx.authority
        .record_upload(who, SPACE, request_id, chunk, at_ms)
        .unwrap();
    chunk
}

fn live(fx: &mut Fixture, now_ms: i64) -> Vec<ChunkId> {
    fx.authority
        .collect(now_ms, &keep_versions())
        .unwrap()
        .live
        .into_iter()
        .collect()
}

/// A grant of `version` of `entry` in `space`: the chunks it would sign, or the refusal.
fn grant(
    fx: &mut Fixture,
    who: &Principal,
    space: &str,
    entry: u64,
    version: VersionId,
) -> Result<usize, ErrorCode> {
    let request = Request::Grant {
        space: space.into(),
        entry: EntryId(entry),
        version,
        chunks: vec![0],
    };
    match fx.authority.handle(who, request, UPLOADED_MS) {
        Ok(Outcome::Sign { chunks }) => Ok(chunks.len()),
        Ok(Outcome::Reply(reply)) => panic!("expected grants, got {reply:?}"),
        Err(error) => Err(error.code),
    }
}

fn history_ids(
    fx: &mut Fixture,
    who: &Principal,
    space: &str,
    at: &str,
) -> Result<Vec<VersionId>, ErrorCode> {
    let request = Request::History {
        space: space.into(),
        path: path(at),
    };
    match fx.call(who, request)? {
        Response::History { versions } => Ok(versions.iter().map(|version| version.id).collect()),
        other => panic!("expected history, got {other:?}"),
    }
}

/// Uploads are grouped by uploader and request id: bob uploading fresh chunks under the request
/// id alice used does not keep her stale uploads live, while her own fresh upload does.
// contract: C15, S7
#[test]
fn another_principals_upload_under_the_same_request_id_keeps_no_upload_of_mine() {
    let mut fx = Fixture::new();
    fx.set_acl("x", "user:bob", Some(Perm::Write));
    record(&mut fx, &alice(), "shared", b"alice's", UPLOADED_MS);
    let later = UPLOADED_MS + GRACE_MS + 1;
    let bobs = record(&mut fx, &bob(), "shared", b"bob's", later - 1);
    assert_eq!(live(&mut fx, later), [bobs.id]);

    let kept = record(&mut fx, &alice(), "again", b"kept", UPLOADED_MS);
    let fresh = record(&mut fx, &alice(), "again", b"fresh", later - 1);
    let mut expected = vec![bobs.id, kept.id, fresh.id];
    expected.sort();
    assert_eq!(live(&mut fx, later), expected);
}

/// Odd request ids in the upload path (empty, 1 MiB long, NUL, a slash, `..`, non-ASCII) are
/// ordinary keys: each commit's uploads stay live through the grace and none after it.
// contract: C15
#[test]
fn odd_request_ids_keep_uploads_live_for_the_grace_only() {
    let mut fx = Fixture::new();
    let long = "r".repeat(1 << 20);
    let ids = ["", long.as_str(), "\0", "a/b", "..", "\u{202e}ünï"];
    let mut uploaded: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(n, id)| {
            record(
                &mut fx,
                &alice(),
                id,
                format!("chunk {n}").as_bytes(),
                UPLOADED_MS,
            )
            .id
        })
        .collect();
    uploaded.sort();
    assert_eq!(live(&mut fx, UPLOADED_MS + GRACE_MS), uploaded);
    assert_eq!(live(&mut fx, UPLOADED_MS + GRACE_MS + 1), []);
}

/// bob reads `f` from v1 on. A fork of the space has the head only: bob sees just that, and no
/// version id of the parent, visible to him there or not, reaches the fork's entry.
// contract: C13, S8
#[test]
fn a_fork_shows_no_parent_version_to_a_late_reader() {
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"one");
    fx.set_acl("f", "user:bob", Some(Perm::Read));
    let v2 = fx
        .commit(&alice(), file.id, "r2", Some(v1.id), false, &v1.chunks)
        .unwrap();
    let v3 = fx
        .commit(&alice(), file.id, "r3", Some(v2.id), false, &v1.chunks)
        .unwrap();
    assert_eq!(grant(&mut fx, &bob(), SPACE, file.id.0, v2.id), Ok(1));
    assert_eq!(
        grant(&mut fx, &bob(), SPACE, file.id.0, v1.id),
        Err(ErrorCode::NotFound)
    );

    let fork = Request::Fork {
        space: SPACE.into(),
        name: "copy".into(),
    };
    fx.call(&alice(), fork).unwrap();
    let copy = match fx.call(
        &bob(),
        Request::Stat {
            space: "copy".into(),
            path: path("f"),
        },
    ) {
        Ok(Response::Entry { entry }) => entry,
        other => panic!("expected the fork's entry, got {other:?}"),
    };
    let head = copy.head.expect("the fork copies the head");
    assert_eq!(history_ids(&mut fx, &bob(), "copy", "f"), Ok(vec![head]));
    for version in [v1.id, v2.id, v3.id] {
        assert_eq!(
            grant(&mut fx, &bob(), "copy", copy.id.0, version),
            Err(ErrorCode::NotFound)
        );
        assert_eq!(
            grant(&mut fx, &alice(), "copy", copy.id.0, version),
            Err(ErrorCode::NotFound)
        );
    }
    fx.commit(&alice(), file.id, "r4", None, false, &[]).unwrap();
    assert_eq!(history_ids(&mut fx, &bob(), "copy", "f"), Ok(vec![head]));
}

/// alice deletes her space and bob creates one of the same name. As its admin he reaches none of
/// her entries or versions: not by entry id, restore, base or replay, and her chunk is not his.
// contract: S7, S8, C15
#[test]
fn a_space_recreated_under_the_same_name_reaches_nothing_of_the_old_one() {
    let mut fx = Fixture::new();
    let (file, v1) = fx.file("f", b"secret");
    fx.call(&alice(), Request::DeleteSpace { space: SPACE.into() })
        .unwrap();
    fx.call(&bob(), Request::CreateSpace { space: SPACE.into() })
        .unwrap();
    assert_eq!(
        grant(&mut fx, &bob(), SPACE, file.id.0, v1.id),
        Err(ErrorCode::NotFound)
    );

    let mine = fx.create(&bob(), "f").unwrap();
    let restore = Request::Restore {
        space: SPACE.into(),
        path: path("f"),
        version: v1.id,
    };
    assert_eq!(fx.call(&bob(), restore), Err(ErrorCode::NotFound));
    let old = v1.chunks[0];
    assert_eq!(
        fx.commit(&bob(), mine.id, "on-v1", Some(v1.id), true, &[old]),
        Err(ErrorCode::Conflict)
    );
    assert_eq!(
        fx.commit(&bob(), mine.id, "bare", None, false, &[old]),
        Err(ErrorCode::Invalid)
    );
    assert_eq!(
        fx.commit(&alice(), file.id, "f", None, false, &[old]),
        Err(ErrorCode::NotFound)
    );
    assert_eq!(history_ids(&mut fx, &bob(), SPACE, "f"), Ok(vec![]));
    assert!(!live(&mut fx, UPLOADED_MS + GRACE_MS).contains(&old.id));
}

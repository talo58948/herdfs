//! Red team, round 15: retention and collection across authority clock changes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod support;

use herdfs_authority::{Authority, Outcome, Policy};
use herdfs_core::proto::{Request, Response};
use herdfs_core::{ChunkId, ChunkRef, SpacePath};
use support::alice;

const TRIALS: usize = 3;

fn call(authority: &mut Authority, request: Request, now_ms: i64) -> Response {
    match authority.handle(&alice(), request, now_ms).unwrap() {
        Outcome::Reply(reply) => reply,
        Outcome::Sign { .. } => panic!("only Grant signs"),
    }
}

/// Three entries with two versions each and three uncommitted uploads. After the last operation,
/// `backward` makes the wall clock jump to zero; collection is called after another 101 ms of wall
/// time. The authority is restarted between the jump and collection.
fn collect_after_clock_change(backward: bool) -> (usize, usize) {
    const FORWARD_MS: i64 = 1_000_000;
    const AGE_MS: i64 = 100;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("authority.db");
    let mut authority = Authority::open(&db).unwrap();
    call(
        &mut authority,
        Request::CreateSpace {
            space: "clock".into(),
        },
        0,
    );
    let mut wall_ms = FORWARD_MS;
    for trial in 0..TRIALS {
        let path = SpacePath::try_from(format!("f{trial}")).unwrap();
        let Response::Entry { entry } = call(
            &mut authority,
            Request::Create {
                space: "clock".into(),
                path,
                mode: 0o644,
            },
            wall_ms,
        ) else {
            panic!("create answers with an entry")
        };
        for version in 0..2 {
            wall_ms += 1;
            call(
                &mut authority,
                Request::Commit {
                    space: "clock".into(),
                    entry: entry.id,
                    request_id: format!("{trial}-{version}"),
                    base: None,
                    guarded: false,
                    chunks: Vec::new(),
                    mtime_ms: None,
                },
                wall_ms,
            );
        }
        wall_ms += 1;
    }
    let mut uploads = Vec::new();
    for trial in 0..TRIALS {
        wall_ms += 1;
        let bytes = [trial as u8];
        let chunk = ChunkRef {
            id: ChunkId::of(&bytes),
            len: 1,
        };
        authority
            .record_upload(&alice(), "clock", chunk, wall_ms)
            .unwrap();
        uploads.push(chunk.id);
    }
    drop(authority);

    let mut authority = Authority::open(&db).unwrap();
    let collect_at = if backward {
        AGE_MS + 1
    } else {
        wall_ms + AGE_MS + 1
    };
    let policy = Policy {
        versions: 1,
        max_age_ms: AGE_MS,
        grace_ms: AGE_MS,
    };
    let collected = authority.collect(collect_at, &policy).unwrap();
    let stale_uploads = uploads
        .iter()
        .filter(|chunk| collected.live.contains(chunk))
        .count();
    (collected.pruned, stale_uploads)
}

/// Repro: after a forward clock jump, the authority restarts and the clock jumps backward. One
/// retention/grace period of wall time later, all three old versions and uploads are still kept:
/// `next_ms` advances only one millisecond per request while the wall clock catches up. With no
/// backward jump, the same ages prune every old version and expire every upload.
// contract: C14, C15, S7
#[test]
#[ignore = "ROUND15: backward clock jump pauses retention and upload grace"]
fn backward_clock_jump_does_not_pause_retention_or_collection_grace() {
    assert_eq!(
        collect_after_clock_change(false),
        (TRIALS, 0),
        "control without a backward jump"
    );
    let (pruned, stale_uploads) = collect_after_clock_change(true);
    assert_eq!(
        (pruned, stale_uploads),
        (TRIALS, 0),
        "pruned old versions and live uploads after the backward jump"
    );
}

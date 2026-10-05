//! Red team, round 42: retained versions created during a corrected forward clock step.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::{Outcome, Policy};
use herdfs_core::{
    ChunkId, ChunkRef,
    proto::{EntryId, Request, Response},
};
use support::{Fixture, SPACE, alice};

const CORRECTED_MS: i64 = 10_000;
const FORWARD_MS: i64 = 1_000_000;
const AGE_MS: i64 = 100;
const INTERVAL_MS: i64 = 20;
const POLICY: Policy = Policy {
    versions: 1,
    max_age_ms: AGE_MS,
    grace_ms: AGE_MS,
};

fn commit_at(fx: &mut Fixture, entry: EntryId, trial: usize, version: usize, at_ms: i64) -> ChunkId {
    let bytes = format!("round 42 retention {trial} {version}");
    let chunk = ChunkRef {
        id: ChunkId::of(bytes.as_bytes()),
        len: u32::try_from(bytes.len()).unwrap(),
    };
    let request_id = format!("{trial}-{version}");
    fx.authority
        .record_upload(&alice(), SPACE, &request_id, chunk, at_ms)
        .unwrap();
    let request = Request::Commit {
        space: SPACE.into(),
        entry,
        request_id,
        base: None,
        guarded: false,
        chunks: vec![chunk],
        mtime_ms: None,
    };
    assert!(matches!(
        fx.authority.handle(&alice(), request, at_ms),
        Ok(Outcome::Reply(Response::Version { .. }))
    ));
    chunk.id
}

fn old_chunks_live(forward_step: bool, trial: usize) -> usize {
    let mut fx = Fixture::new();
    let file = fx.create(&alice(), "f").unwrap();
    let first = if forward_step { FORWARD_MS } else { CORRECTED_MS };
    let old = [
        commit_at(&mut fx, file.id, trial, 1, first),
        commit_at(&mut fx, file.id, trial, 2, first + 1),
    ];
    let head = commit_at(&mut fx, file.id, trial, 3, CORRECTED_MS + 1);

    let mut live = Default::default();
    let mut at = CORRECTED_MS + INTERVAL_MS;
    while at <= CORRECTED_MS + AGE_MS + INTERVAL_MS {
        live = fx.authority.collect(at, &POLICY).unwrap().live;
        at += INTERVAL_MS;
    }
    assert!(live.contains(&head), "trial {trial}: collection lost the head");
    old.iter().filter(|chunk| live.contains(chunk)).count()
}

/// Once a forward clock step is corrected, non-head versions age from the correction just as
/// uploads do; regular collections must drop them after the retention age, while the head stays.
// contract: C14, C15, S7
#[test]
fn corrected_future_versions_leave_retention_on_schedule() {
    let mut failures = 0;
    for trial in 0..3 {
        assert_eq!(
            old_chunks_live(false, trial),
            0,
            "trial {trial}: normal-time control remained retained"
        );
        let remained = old_chunks_live(true, trial);
        eprintln!("trial {trial}: {remained}/2 future-stamped chunks remained");
        failures += usize::from(remained != 0);
    }
    assert_eq!(
        failures, 0,
        "future-stamped versions remained in {failures}/3 trials"
    );
}

//! Red team, round 38: uploads stamped during a corrected forward clock jump.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::{Outcome, Policy};
use herdfs_core::{
    ChunkId, ChunkRef,
    proto::{Request, Response},
};
use support::{Fixture, SPACE, alice};

const CORRECTED_MS: i64 = 10_000;
const FORWARD_MS: i64 = 1_000_000;
const GRACE_MS: i64 = 100;
const INTERVAL_MS: i64 = 20;
const POLICY: Policy = Policy {
    versions: 1,
    max_age_ms: 0,
    grace_ms: GRACE_MS,
};

fn deleted_upload(at_ms: i64, trial: usize, label: &str) -> (Fixture, ChunkId) {
    let mut fx = Fixture::new();
    let file = fx.create(&alice(), "f").unwrap();
    let bytes = format!("round 38 {label} {trial}");
    let chunk = ChunkRef {
        id: ChunkId::of(bytes.as_bytes()),
        len: u32::try_from(bytes.len()).unwrap(),
    };
    let request_id = format!("{label}-{trial}");
    fx.authority
        .record_upload(&alice(), SPACE, &request_id, chunk, at_ms)
        .unwrap();
    assert!(matches!(
        fx.authority.handle(
            &alice(),
            Request::Commit {
                space: SPACE.into(),
                entry: file.id,
                request_id,
                base: None,
                guarded: false,
                chunks: vec![chunk],
                mtime_ms: None,
            },
            CORRECTED_MS + 1,
        ),
        Ok(Outcome::Reply(Response::Version { .. }))
    ));
    assert_eq!(
        fx.authority.handle(
            &alice(),
            Request::DeleteSpace { space: SPACE.into() },
            CORRECTED_MS + 2,
        ),
        Ok(Outcome::Reply(Response::Done))
    );
    (fx, chunk.id)
}

/// Collections every `INTERVAL_MS`, from just after the deletion through grace plus one interval
/// past it; whether the deleted upload's chunk is still live after the last.
fn live_after_regular_collections(fx: &mut Fixture, chunk: ChunkId) -> bool {
    let deleted = CORRECTED_MS + 2;
    let mut at = deleted + INTERVAL_MS;
    let mut live = true;
    while at <= deleted + GRACE_MS + INTERVAL_MS {
        live = fx.authority.collect(at, &POLICY).unwrap().live.contains(&chunk);
        at += INTERVAL_MS;
    }
    live
}

/// After a forward wall-clock step is corrected, an upload made during the step ages from the
/// first collection after the correction: with collections every interval, a deleted space's
/// bytes leave the live set within grace plus one interval, as a normal upload's do.
// contract: C15, S7
#[test]
fn a_corrected_forward_clock_step_does_not_keep_deleted_uploads_live() {
    for trial in 0..3 {
        let (mut control, control_chunk) = deleted_upload(CORRECTED_MS, trial, "control");
        assert!(
            !live_after_regular_collections(&mut control, control_chunk),
            "trial {trial}: normal-time control remained live"
        );
        let (mut jumped, jumped_chunk) = deleted_upload(FORWARD_MS, trial, "jumped");
        assert!(
            !live_after_regular_collections(&mut jumped, jumped_chunk),
            "trial {trial}: a future-stamped upload remained live"
        );
    }
}

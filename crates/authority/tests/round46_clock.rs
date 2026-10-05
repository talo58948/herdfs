//! Red team, round 46: request-id memory after a corrected forward clock step.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::{Outcome, Policy};
use herdfs_core::proto::{EntryId, Request, Response, VersionId};
use support::{Fixture, SPACE, alice};

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const CORRECTED_MS: i64 = 10_000;
const FORWARD_MS: i64 = CORRECTED_MS + 100 * DAY_MS;
const REQUEST_MEMORY_MS: i64 = 30 * DAY_MS;
const POLICY: Policy = Policy {
    versions: u32::MAX,
    max_age_ms: 0,
    grace_ms: 0,
};

fn commit_at(fx: &mut Fixture, entry: EntryId, request_id: &str, at_ms: i64) -> VersionId {
    let request = Request::Commit {
        space: SPACE.into(),
        entry,
        request_id: request_id.into(),
        base: None,
        guarded: false,
        chunks: Vec::new(),
        mtime_ms: None,
    };
    let Outcome::Reply(Response::Version { version }) =
        fx.authority.handle(&alice(), request, at_ms).unwrap()
    else {
        panic!("commit did not return a version")
    };
    version.id
}

fn replay_after_memory(forward_step: bool, trial: usize) -> bool {
    let mut fx = Fixture::new();
    let file = fx.create(&alice(), &format!("f-{trial}")).unwrap();
    let request_id = format!("request-{trial}");
    let committed_at = if forward_step { FORWARD_MS } else { CORRECTED_MS };
    let first = commit_at(&mut fx, file.id, &request_id, committed_at);

    // The first collection after correction brings every future bookkeeping timestamp back
    // to the authority's current time. A later collection may then age it normally.
    fx.authority.collect(CORRECTED_MS, &POLICY).unwrap();
    fx.authority
        .collect(CORRECTED_MS + REQUEST_MEMORY_MS + 1, &POLICY)
        .unwrap();
    commit_at(
        &mut fx,
        file.id,
        &request_id,
        CORRECTED_MS + REQUEST_MEMORY_MS + 2,
    ) == first
}

/// A corrected forward step must not pin request ids to the discarded future: after the
/// documented 30-day memory, reuse creates a new version just as the normal-time control does.
// contract: C4a (docs/architecture.md: request-id memory uses the authority clock, and
// timestamps left in the future are corrected)
#[test]
fn corrected_future_request_ids_age_out_of_memory() {
    let mut stuck = 0;
    for trial in 0..3 {
        assert!(
            !replay_after_memory(false, trial),
            "trial {trial}: normal-time control was still replayed"
        );
        let replayed = replay_after_memory(true, trial);
        eprintln!("trial {trial}: future-stamped request id replayed={replayed}");
        stuck += usize::from(replayed);
    }
    assert_eq!(stuck, 0, "{stuck}/3 future-stamped request ids outlived memory");
}

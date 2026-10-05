//! Red team, round 44: commit replay memory across a corrected forward clock step.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::Policy;
use support::{Fixture, alice};

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const POLICY: Policy = Policy {
    versions: 16,
    max_age_ms: 0,
    grace_ms: 0,
};

fn replay_after(collection_ms: i64, trial: usize) -> bool {
    let mut fx = Fixture::new();
    let (file, base) = fx.file("f", format!("base {trial}").as_bytes());
    let request_id = format!("lost-{trial}");
    let lost = fx
        .commit(&alice(), file.id, &request_id, Some(base.id), false, &base.chunks)
        .unwrap();
    let newer = fx
        .commit(&alice(), file.id, &format!("newer-{trial}"), None, false, &[])
        .unwrap();
    fx.authority.collect(collection_ms, &POLICY).unwrap();

    let replay = fx
        .commit(&alice(), file.id, &request_id, Some(base.id), false, &base.chunks)
        .unwrap();
    let history = fx.history(&alice(), "f").unwrap();
    let head = history.last().unwrap();
    let duplicated = replay.id != lost.id;
    if duplicated {
        assert_eq!(head.id, replay.id, "the retry did not become the head");
        assert_eq!(
            head.chunks, base.chunks,
            "the retry did not restore the old bytes"
        );
        assert_eq!(history.len(), 4, "the retry did not add exactly one version");
    } else {
        assert_eq!(head.id, newer.id, "the replay changed the control head");
        assert_eq!(history.len(), 3, "the control replay added a version");
    }
    duplicated
}

/// A brief forward clock step during collection must not make an acknowledged request look more
/// than 30 days old. After the clock is corrected, retrying its lost response must return the
/// original version, not commit the old bytes again over a newer head. A 29-day step is the
/// passing control.
// contract: C4a, C5 (docs/architecture.md Limits)
#[test]
#[ignore = "ROUND44: a forward clock step forgets a recent commit replay"]
fn a_corrected_clock_does_not_forget_a_recent_commit() {
    let mut duplicated = 0;
    for trial in 0..3 {
        assert!(
            !replay_after(29 * DAY_MS, trial),
            "trial {trial}: under-window control was forgotten"
        );
        let lost = replay_after(31 * DAY_MS, trial);
        eprintln!("trial {trial}: replay committed again={lost}");
        duplicated += usize::from(lost);
    }
    assert_eq!(duplicated, 0, "{duplicated}/3 retries committed again");
}

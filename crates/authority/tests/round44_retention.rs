//! Red team, round 44: retention across a corrected forward clock step.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::Policy;
use support::{Fixture, alice};

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const POLICY: Policy = Policy {
    versions: 1,
    max_age_ms: 30 * DAY_MS,
    grace_ms: 0,
};

fn first_version_survives(collection_ms: i64, trial: usize) -> bool {
    let mut fx = Fixture::new();
    let (file, first) = fx.file("f", format!("first {trial}").as_bytes());
    fx.commit(&alice(), file.id, &format!("head-{trial}"), None, false, &[])
        .unwrap();
    fx.authority.collect(collection_ms, &POLICY).unwrap();
    fx.history(&alice(), "f")
        .unwrap()
        .iter()
        .any(|version| version.id == first.id)
}

/// A brief forward clock step during collection must not prune a version made moments before;
/// after correction it is still younger than the configured 30 days. A 29-day step is the
/// passing control.
// contract: C14
#[test]
#[ignore = "ROUND44: a forward clock step prunes recent retained history"]
fn a_corrected_clock_does_not_prune_recent_history() {
    let mut pruned = 0;
    for trial in 0..3 {
        assert!(
            first_version_survives(29 * DAY_MS, trial),
            "trial {trial}: under-age control was pruned"
        );
        let survived = first_version_survives(31 * DAY_MS, trial);
        eprintln!("trial {trial}: recent version survived={survived}");
        pruned += usize::from(!survived);
    }
    assert_eq!(pruned, 0, "{pruned}/3 recent versions were pruned");
}

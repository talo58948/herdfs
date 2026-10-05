//! Round 54: guarded-conflict retry and replay after the commit split.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_core::proto::ErrorCode;
use support::{Fixture, alice};

/// A guarded loser consumes neither its upload nor request id. Retrying it unguarded lands once,
/// while replaying with changed fields returns that same version.
// contract: C5, C6, S6
#[test]
fn a_guarded_conflict_can_retry_unguarded_without_losing_provenance() {
    for trial in 0..3 {
        let mut fixture = Fixture::new();
        let (file, base) = fixture.file(&format!("f-{trial}"), b"base");
        let uploaded = fixture
            .upload(&alice(), format!("loser-{trial}").as_bytes())
            .unwrap();
        let winner = fixture
            .commit(
                &alice(),
                file.id,
                &format!("winner-{trial}"),
                Some(base.id),
                true,
                &[],
            )
            .unwrap();
        let request = format!("loser-{trial}");

        assert_eq!(
            fixture.commit(&alice(), file.id, &request, Some(base.id), true, &[uploaded]),
            Err(ErrorCode::Conflict),
            "trial {trial}"
        );
        let retried = fixture
            .commit(&alice(), file.id, &request, Some(base.id), false, &[uploaded])
            .unwrap();
        assert_eq!(
            fixture.commit(&alice(), file.id, &request, None, true, &[]),
            Ok(retried.clone()),
            "replay control {trial}"
        );
        assert_eq!(
            fixture.history(&alice(), &format!("f-{trial}")).unwrap(),
            [base, winner, retried],
            "trial {trial}"
        );
    }
}

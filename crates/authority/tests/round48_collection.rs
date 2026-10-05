//! Round 48: upload ordering when a collection races a repeated upload.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::Policy;
use herdfs_core::{ChunkId, ChunkRef};
use support::{Fixture, SPACE, alice};

const KEEP_UPLOADS: Policy = Policy {
    versions: 1,
    max_age_ms: 0,
    grace_ms: 10_000,
};

fn chunk(bytes: &[u8]) -> ChunkRef {
    ChunkRef {
        id: ChunkId::of(bytes),
        len: bytes.len().try_into().unwrap(),
    }
}

/// Replacing the same `(principal, chunk, request)` upload after a collection gives it a later
/// sequence even when the corrected clock is earlier. The sweep's recheck sees it; an untouched
/// upload from before the mark is the control.
// contract: C15, S7
#[test]
fn a_repeated_upload_after_the_mark_is_fresh_by_order_not_time() {
    for trial in 0..3 {
        let mut fixture = Fixture::new();
        let repeated = chunk(format!("repeated-{trial}").as_bytes());
        let old = chunk(format!("old-{trial}").as_bytes());
        fixture
            .authority
            .record_upload(&alice(), SPACE, "same", repeated, 5_000)
            .unwrap();
        fixture
            .authority
            .record_upload(&alice(), SPACE, "old", old, 5_000)
            .unwrap();
        let mark = fixture
            .authority
            .collect(6_000, &KEEP_UPLOADS)
            .unwrap()
            .upload_mark;

        fixture
            .authority
            .record_upload(&alice(), SPACE, "same", repeated, 1_000)
            .unwrap();
        let fresh = fixture.authority.uploaded_after(mark).unwrap();
        assert!(fresh.contains(&repeated.id), "trial {trial}");
        assert!(!fresh.contains(&old.id), "old control, trial {trial}");
    }
}

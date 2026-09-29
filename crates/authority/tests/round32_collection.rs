//! Red team, round 32: exact retention and upload-grace boundaries.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::Policy;
use herdfs_core::{ChunkId, ChunkRef};
use support::{Fixture, SPACE, alice};

const AGE_MS: i64 = 100;

fn policy() -> Policy {
    Policy {
        versions: 1,
        max_age_ms: AGE_MS,
        grace_ms: AGE_MS,
    }
}

/// A non-head version one millisecond younger than the retention age stays, while one exactly
/// that old has stopped being younger and must be pruned.
// contract: C14
#[test]
fn exact_retention_age_is_not_retained_as_younger() {
    let mut retained = 0;
    for trial in 0..3 {
        let mut fx = Fixture::new();
        let (file, old) = fx.file("f", format!("old {trial}").as_bytes());
        fx.commit(&alice(), file.id, "new", None, false, &[]).unwrap();

        let control = fx
            .authority
            .collect(old.created_ms + AGE_MS - 1, &policy())
            .unwrap();
        assert_eq!(control.pruned, 0, "trial {trial}: younger control was pruned");
        let boundary = fx.authority.collect(old.created_ms + AGE_MS, &policy()).unwrap();
        retained += usize::from(boundary.pruned == 0);
    }
    assert_eq!(retained, 0, "{retained}/3 exact-age versions were retained");
}

/// An upload one millisecond younger than the grace stays live, while one exactly grace old is no
/// longer a recent upload and must leave the live set.
// contract: C15
#[test]
fn exact_upload_grace_is_not_recent() {
    let mut retained = 0;
    for trial in 0..3 {
        let mut fx = Fixture::new();
        let bytes = format!("upload {trial}");
        let chunk = ChunkRef {
            id: ChunkId::of(bytes.as_bytes()),
            len: u32::try_from(bytes.len()).unwrap(),
        };
        let uploaded_ms = 10_000;
        fx.authority
            .record_upload(&alice(), SPACE, "pending", chunk, uploaded_ms)
            .unwrap();

        let control = fx.authority.collect(uploaded_ms + AGE_MS - 1, &policy()).unwrap();
        assert!(
            control.live.contains(&chunk.id),
            "trial {trial}: younger control was dropped"
        );
        let boundary = fx.authority.collect(uploaded_ms + AGE_MS, &policy()).unwrap();
        retained += usize::from(boundary.live.contains(&chunk.id));
    }
    assert_eq!(retained, 0, "{retained}/3 exact-grace uploads remained live");
}

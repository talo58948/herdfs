//! Red team, round 40: an author's retained version across revocation and re-grant.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_core::proto::{ErrorCode, Perm};
use support::{Fixture, alice, bob};

/// A re-granted reader still sees a version they authored before revocation. The pre-access
/// version stays hidden and the current head stays readable as controls.
// contract: S8
#[test]
#[ignore = "ROUND40: a re-granted reader cannot read their own retained version"]
fn a_regranted_author_can_read_their_retained_version() {
    let mut failures = 0;
    for trial in 0..3 {
        let mut fx = Fixture::new();
        let (file, before_access) = fx.file("f", b"before access");
        fx.set_acl("f", "user:bob", Some(Perm::Write));
        let mine_chunk = fx.upload(&bob(), format!("mine {trial}").as_bytes()).unwrap();
        let mine = fx
            .commit(
                &bob(),
                file.id,
                &format!("mine-{trial}"),
                None,
                false,
                &[mine_chunk],
            )
            .unwrap();
        let head = fx
            .commit(&alice(), file.id, &format!("head-{trial}"), None, false, &[])
            .unwrap();
        fx.set_acl("f", "user:bob", None);
        fx.set_acl("f", "user:bob", Some(Perm::Read));

        let replay = fx
            .commit(&bob(), file.id, &format!("mine-{trial}"), None, false, &[])
            .unwrap();
        assert_eq!(replay.id, mine.id, "the authority remembers bob authored it");
        assert_eq!(
            fx.grant(&bob(), file.id, before_access.id, &[0]),
            Err(ErrorCode::NotFound)
        );
        assert_eq!(fx.grant(&bob(), file.id, head.id, &[]), Ok(Vec::new()));

        let history = fx.history(&bob(), "f").unwrap();
        let listed = history.iter().any(|version| version.id == mine.id);
        let granted = fx.grant(&bob(), file.id, mine.id, &[0]);
        eprintln!("trial {trial}: listed={listed}, grant={granted:?}");
        failures += usize::from(!listed || granted != Ok(vec![mine_chunk.id]));
    }
    assert_eq!(failures, 0, "re-granted authors denied in {failures}/3 trials");
}

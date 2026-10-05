//! Round 56: S8 history when read access transfers between overlapping ACL rows.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_core::proto::{ErrorCode, Perm, Request, Response};
use support::{Fixture, SPACE, alice, bob, path as space_path};

fn overlapping_history(trial: usize, overlap: bool) -> (bool, bool) {
    let mut fixture = Fixture::new();
    let dir = format!("d-{trial}");
    let path = format!("{dir}/f");
    fixture.mkdir(&alice(), &dir).unwrap();
    let (file, before) = fixture.file(&path, b"before access");
    fixture.set_acl(&dir, "user:bob", Some(Perm::Write));
    let chunk = fixture
        .upload(&alice(), format!("visible-{trial}").as_bytes())
        .unwrap();
    let visible = fixture
        .commit(
            &alice(),
            file.id,
            &format!("visible-{trial}"),
            None,
            false,
            &[chunk],
        )
        .unwrap();
    if !overlap {
        fixture.set_acl(&dir, "user:bob", None);
    }
    fixture.set_acl(&path, "user:bob", Some(Perm::Write));
    if overlap {
        fixture.set_acl(&dir, "user:bob", None);
    }
    let head = fixture
        .commit(&alice(), file.id, &format!("head-{trial}"), None, false, &[])
        .unwrap();
    let history = fixture.history(&bob(), &path).unwrap();
    let granted = fixture.grant(&bob(), file.id, visible.id, &[0]) == Ok(vec![chunk.id]);
    let restored = matches!(
        fixture.call(
            &bob(),
            Request::Restore {
                space: SPACE.into(),
                path: space_path(&path),
                version: visible.id,
            },
        ),
        Ok(Response::Version { .. })
    );
    let based = fixture
        .commit(
            &bob(),
            file.id,
            &format!("based-{trial}"),
            Some(visible.id),
            false,
            &visible.chunks,
        )
        .is_ok();
    let listed = history.iter().any(|version| version.id == visible.id);
    eprintln!(
        "trial {trial}, overlap={overlap}: listed={listed}, granted={granted}, restored={restored}, based={based}"
    );
    assert_eq!(
        fixture.grant(&bob(), file.id, before.id, &[0]),
        Err(ErrorCode::NotFound),
        "pre-access control, trial {trial}"
    );
    (
        listed && granted && restored && based,
        history == [head] && !granted && !restored && !based,
    )
}

/// Replacing an old ancestor grant with an already-applying entry grant must preserve history from
/// when the principal first gained uninterrupted read access across history, grants, restore and
/// commit bases; a real revoke/re-grant is the control.
// contract: C14, S8
#[test]
#[ignore = "ROUND56: overlapping ACL replacement resets uninterrupted history access"]
fn an_overlapping_grant_keeps_the_original_history_boundary() {
    let mut lost = 0;
    for trial in 0..3 {
        let (kept, _) = overlapping_history(trial, true);
        lost += usize::from(!kept);
        let (_, reset) = overlapping_history(trial + 10, false);
        assert!(reset, "loss and re-grant control, trial {trial}");
    }
    assert_eq!(lost, 0, "{lost}/3 overlapping grants lost readable history");
}

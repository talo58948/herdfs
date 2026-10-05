//! Round 52: commit provenance and replay boundaries after the implementation split.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_authority::Policy;
use herdfs_core::{
    ChunkId, ChunkRef,
    proto::{ErrorCode, Request, Response},
};
use support::{Fixture, SPACE, alice};

const HEAD_ONLY: Policy = Policy {
    versions: 1,
    max_age_ms: 0,
    grace_ms: 0,
};

/// A rejected commit consumes neither its request id nor an earlier upload: retrying the same
/// logical commit with a valid manifest lands once, and later replay ignores changed fields.
// contract: C4a, C5, S6
#[test]
fn rejected_commit_can_be_corrected_without_losing_replay_or_provenance() {
    for trial in 0..3 {
        let mut fixture = Fixture::new();
        let file = fixture.create(&alice(), &format!("f-{trial}")).unwrap();
        let uploaded = fixture
            .upload(&alice(), format!("uploaded-{trial}").as_bytes())
            .unwrap();
        let absent = format!("absent-{trial}");
        let absent = ChunkRef {
            id: ChunkId::of(absent.as_bytes()),
            len: absent.len().try_into().unwrap(),
        };
        let request = format!("request-{trial}");

        assert_eq!(
            fixture.commit(&alice(), file.id, &request, None, false, &[absent]),
            Err(ErrorCode::Invalid)
        );
        let landed = fixture
            .commit(&alice(), file.id, &request, None, false, &[uploaded])
            .unwrap();
        assert_eq!(
            fixture.commit(&alice(), file.id, &request, None, true, &[]),
            Ok(landed.clone()),
            "trial {trial}"
        );
        assert_eq!(
            fixture.history(&alice(), &format!("f-{trial}")).unwrap(),
            [landed]
        );
    }
}

/// A fork keeps its copied head's chunks usable only through the fork's own base. Once the parent
/// base is pruned, the shared object is not provenance for a new parent commit.
// contract: C13, C14, C15, S6
#[test]
fn retained_fork_chunk_does_not_become_parent_commit_provenance() {
    for trial in 0..3 {
        let mut fixture = Fixture::new();
        let (file, first) = fixture.file(&format!("f-{trial}"), format!("one-{trial}").as_bytes());
        fixture
            .call(
                &alice(),
                Request::Fork {
                    space: SPACE.into(),
                    name: format!("copy-{trial}"),
                },
            )
            .unwrap();
        let second = fixture
            .commit(&alice(), file.id, &format!("head-{trial}"), None, false, &[])
            .unwrap();
        let collected = fixture
            .authority
            .collect(second.created_ms + 1, &HEAD_ONLY)
            .unwrap();
        assert!(collected.live.contains(&first.chunks[0].id), "fork control");
        assert_eq!(
            fixture.commit(
                &alice(),
                file.id,
                &format!("reuse-{trial}"),
                Some(first.id),
                false,
                &first.chunks,
            ),
            Err(ErrorCode::Conflict),
            "trial {trial}"
        );
        assert_eq!(
            fixture.commit(
                &alice(),
                file.id,
                &format!("bare-{trial}"),
                None,
                false,
                &first.chunks,
            ),
            Err(ErrorCode::Invalid),
            "trial {trial}"
        );

        let Response::Entry { entry: copy } = fixture
            .call(
                &alice(),
                Request::Stat {
                    space: format!("copy-{trial}"),
                    path: support::path(&format!("f-{trial}")),
                },
            )
            .unwrap()
        else {
            panic!("stat answers with an entry")
        };
        let copy_base = copy.head.unwrap();
        let request = Request::Commit {
            space: format!("copy-{trial}"),
            entry: copy.id,
            request_id: format!("copy-write-{trial}"),
            base: Some(copy_base),
            guarded: true,
            chunks: first.chunks.clone(),
            mtime_ms: None,
        };
        assert!(matches!(
            fixture.call(&alice(), request),
            Ok(Response::Version { .. })
        ));
    }
}

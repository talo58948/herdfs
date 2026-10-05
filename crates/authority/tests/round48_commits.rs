//! Round 48: replay ordering and S8 visibility after the commit implementation split.
#![allow(clippy::unwrap_used, reason = "test code")]

mod support;

use herdfs_core::{
    ChunkId, ChunkRef,
    proto::{ErrorCode, Perm, Request, Response, VersionId},
};
use support::{Fixture, SPACE, alice, bob, path};

fn restore(version: VersionId) -> Request {
    Request::Restore {
        space: SPACE.into(),
        path: path("f"),
        version,
    }
}

/// A replay is recognized before current ACL, base, guard and provenance checks. Once read/write
/// is granted again, the author can list, grant and restore that retained version, but not an
/// older version from before their original access.
// contract: C4a, C14, S2, S6, S8
#[test]
fn replay_and_author_visibility_survive_every_later_check() {
    for trial in 0..3 {
        let mut fixture = Fixture::new();
        let (file, hidden) = fixture.file("f", b"before bob");
        fixture.set_acl("f", "user:bob", Some(Perm::Write));
        let mine_chunk = fixture
            .upload(&bob(), format!("mine-{trial}").as_bytes())
            .unwrap();
        let mine = fixture
            .commit(&bob(), file.id, "mine", None, false, &[mine_chunk])
            .unwrap();
        let head = fixture
            .commit(&alice(), file.id, "new-head", None, false, &[])
            .unwrap();
        fixture.set_acl("f", "user:bob", None);

        let guessed = b"not uploaded by bob";
        let replay = Request::Commit {
            space: SPACE.into(),
            entry: file.id,
            request_id: "mine".into(),
            base: Some(hidden.id),
            guarded: true,
            chunks: vec![ChunkRef {
                id: ChunkId::of(guessed),
                len: guessed.len().try_into().unwrap(),
            }],
            mtime_ms: Some(i64::MIN),
        };
        assert_eq!(
            fixture.call(&bob(), replay),
            Ok(Response::Version {
                version: mine.clone()
            }),
            "trial {trial}"
        );
        assert_eq!(
            fixture.commit(&bob(), file.id, "fresh", None, false, &[]),
            Err(ErrorCode::Denied),
            "fresh control"
        );

        fixture.set_acl("f", "user:bob", Some(Perm::Write));
        assert_eq!(fixture.history(&bob(), "f").unwrap(), [mine.clone(), head]);
        assert_eq!(
            fixture.grant(&bob(), file.id, hidden.id, &[0]),
            Err(ErrorCode::NotFound)
        );
        assert_eq!(
            fixture.grant(&bob(), file.id, mine.id, &[0]),
            Ok(vec![mine_chunk.id])
        );
        let Response::Version { version: restored } = fixture.call(&bob(), restore(mine.id)).unwrap() else {
            panic!("restore answers with a version")
        };
        assert_eq!(restored.chunks, mine.chunks);
    }
}

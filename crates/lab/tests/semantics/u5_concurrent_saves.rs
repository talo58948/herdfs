//! U5: one file open on two clients, and both save.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::thread;

use anyhow::Result;
use herdfs_core::proto::{ErrorCode, Request, Response, Version};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::support::{self, VISIBLE_AFTER, history, save, version_bytes};

const FILE: &str = "report.txt";
const CONTENDERS: usize = 8;

fn two_clients() -> Result<Option<Lab>> {
    support::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )
}

#[test]
fn the_later_save_becomes_head_and_both_stay_in_history() -> Result<()> {
    // contract: C5
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let [on_a, on_b] = ["a", "b"].map(|name| lab.mount_path(name).join(FILE));
    save(&on_a, b"draft\n")?;
    thread::sleep(VISIBLE_AFTER);

    let open = |file| OpenOptions::new().write(true).truncate(true).open(file);
    let (mut by_alice, mut by_bob) = (open(&on_a)?, open(&on_b)?);
    by_alice.write_all(b"alice's save\n")?;
    by_bob.write_all(b"bob's save, the later one\n")?;
    // fsync returns once the authority has acknowledged the commit, which orders the two saves.
    by_alice.sync_all()?;
    drop(by_alice);
    by_bob.sync_all()?;
    drop(by_bob);

    thread::sleep(VISIBLE_AFTER);
    for file in [&on_a, &on_b] {
        assert_eq!(fs::read(file)?, b"bob's save, the later one\n");
    }
    let versions = history(&lab, "alice", FILE)?;
    let contents = versions
        .iter()
        .map(|version| version_bytes(&lab, "a", "alice", FILE, version))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        contents,
        [&b"draft\n"[..], b"alice's save\n", b"bob's save, the later one\n"]
    );
    Ok(())
}

#[test]
fn of_guarded_commits_on_one_base_exactly_one_succeeds() -> Result<()> {
    // contract: C6
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    save(&lab.mount_path("a").join(FILE), b"base\n")?;
    let [base] = &history(&lab, "alice", FILE)?[..] else {
        panic!("one save makes one version");
    };

    let outcomes: Vec<_> = thread::scope(|scope| {
        let lab = &lab;
        let contenders: Vec<_> = (0..CONTENDERS)
            .map(|n| scope.spawn(move || lab.rpc(["alice", "bob"][n % 2], &guarded(base, n))))
            .collect();
        contenders.into_iter().map(|contender| contender.join()).collect()
    });
    let (mut won, mut conflicts) = (Vec::new(), 0);
    for outcome in outcomes {
        match outcome.expect("a contender panicked") {
            Ok(Response::Version { version }) => won.push(version),
            Err(error) if error.code == ErrorCode::Conflict => conflicts += 1,
            other => panic!("a guarded commit answered {other:?}"),
        }
    }
    let [winner] = &won[..] else {
        panic!("{} guarded commits won: {won:?}", won.len());
    };
    assert_eq!(conflicts, CONTENDERS - 1);
    let ids: Vec<_> = history(&lab, "alice", FILE)?
        .iter()
        .map(|version| version.id)
        .collect();
    assert_eq!(ids, [base.id, winner.id]);
    Ok(())
}

/// Contender `n`'s commit on `base`: odd ones keep its content, even ones empty the file.
fn guarded(base: &Version, n: usize) -> Request {
    Request::Commit {
        space: SPACE.into(),
        entry: base.entry,
        request_id: format!("contender-{n}"),
        base: Some(base.id),
        guarded: true,
        chunks: if n % 2 == 1 {
            base.chunks.clone()
        } else {
            Vec::new()
        },
    }
}

//! S7: deleting a file, purging its history and deleting a space erase the bytes only they held,
//! and leave the bytes something retained still holds.

use std::fs;

use anyhow::Result;
use herdfs_core::SpacePath;
use herdfs_core::proto::Request;
use herdfs_lab::{SPACE, canary};

use crate::{
    ALICE, SECRET_LEN, assert_stored, head_in, history, random_bytes, read_version, secret, start,
    wait_erased, write, write_direct,
};

// contract: S7
#[test]
fn deleting_a_file_erases_its_bytes_from_the_store() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let (doomed_canary, kept_canary) = (canary(), canary());
    let doomed = write(&lab, "doomed.bin", &secret(&doomed_canary, SECRET_LEN))?;
    let kept_bytes = secret(&kept_canary, SECRET_LEN);
    let kept = write(&lab, "kept.bin", &kept_bytes)?;
    assert_stored(&lab, &doomed.chunks, &doomed_canary)?;
    assert_stored(&lab, &kept.chunks, &kept_canary)?;

    fs::remove_file(lab.mount_path(ALICE).join("doomed.bin"))?;
    wait_erased(&lab, &doomed.chunks, &doomed_canary)?;
    assert_stored(&lab, &kept.chunks, &kept_canary)?;
    assert!(read_version(&lab, ALICE, SPACE, &kept)? == kept_bytes);
    Ok(())
}

// contract: S7
#[test]
fn purging_a_file_erases_its_history_and_keeps_its_head() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let (old_canary, head_canary) = (canary(), canary());
    let old = write(&lab, "report.txt", &secret(&old_canary, SECRET_LEN))?;
    let head_bytes = secret(&head_canary, SECRET_LEN);
    let head = write(&lab, "report.txt", &head_bytes)?;
    assert_stored(&lab, &old.chunks, &old_canary)?;

    let purge = Request::Purge {
        space: SPACE.into(),
        path: SpacePath::try_from("report.txt")?,
    };
    lab.rpc(ALICE, &purge)?;
    assert_eq!(history(&lab, ALICE, "report.txt")?, std::slice::from_ref(&head));
    wait_erased(&lab, &old.chunks, &old_canary)?;
    assert_eq!(lab.head(ALICE, "report.txt")?, head);
    assert_stored(&lab, &head.chunks, &head_canary)?;
    assert!(read_version(&lab, ALICE, SPACE, &head)? == head_bytes);
    Ok(())
}

// contract: S7, C13
#[test]
fn deleting_a_space_erases_its_bytes_but_not_those_a_fork_or_another_space_holds() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let (doomed, fork) = ("doomed", "keeper");
    lab.rpc(ALICE, &Request::CreateSpace { space: doomed.into() })?;
    let forked_bytes = random_bytes(SECRET_LEN);
    write_direct(&lab, doomed, "forked.bin", &forked_bytes)?;
    let fork_request = Request::Fork {
        space: doomed.into(),
        name: fork.into(),
    };
    lab.rpc(ALICE, &fork_request)?;
    // After the fork: bytes the doomed space shares with the lab's space, and bytes only it holds.
    let shared_bytes = random_bytes(SECRET_LEN);
    write_direct(&lab, doomed, "shared.bin", &shared_bytes)?;
    let shared = write(&lab, "shared.bin", &shared_bytes)?;
    let only_canary = canary();
    let only = write_direct(&lab, doomed, "only.bin", &secret(&only_canary, SECRET_LEN))?;
    assert_stored(&lab, &only.chunks, &only_canary)?;

    lab.rpc(ALICE, &Request::DeleteSpace { space: doomed.into() })?;
    wait_erased(&lab, &only.chunks, &only_canary)?;
    assert!(
        head_in(&lab, ALICE, doomed, "forked.bin").is_err(),
        "the space is gone"
    );
    let forked = head_in(&lab, ALICE, fork, "forked.bin")?;
    assert!(read_version(&lab, ALICE, fork, &forked)? == forked_bytes);
    assert!(read_version(&lab, ALICE, SPACE, &shared)? == shared_bytes);
    Ok(())
}

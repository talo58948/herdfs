//! Round 51: a deferred truncate layered with a sibling write on a repeatedly moved entry.

use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::thread;

use anyhow::{Context, Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, Lab, Links, SPACE};

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients, wait_pending};

const TRIALS: usize = 3;

fn deferred_then_written(lab: &mut Lab, trial: usize, moved: bool) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let (a, b) = (lab.mount_path("a").to_owned(), lab.mount_path("b").to_owned());
    let first = format!("deferred-{moved}-{trial}-0");
    save(&a.join(&first), b"old bytes")?;
    thread::sleep(VISIBLE_AFTER);
    let held = OpenOptions::new().read(true).write(true).open(b.join(&first))?;
    let through = PathBuf::from(format!("/proc/self/fd/{}", held.as_raw_fd()));

    let mut current = first;
    let mut replacements = Vec::new();
    if moved {
        for hop in 1..=2 {
            let next = format!("deferred-{moved}-{trial}-{hop}");
            fs::rename(a.join(&current), a.join(&next))?;
            let canary = format!("replacement {hop}").into_bytes();
            save(&a.join(&current), &canary)?;
            replacements.push((current, canary));
            current = next;
        }
        thread::sleep(VISIBLE_AFTER);
        ensure!(held.metadata()?.len() == 9, "held entry follows both moves");
    }

    lab.cut("b", Links::Authority)?;
    drop(OpenOptions::new().write(true).truncate(true).open(&through)?);
    held.write_all_at(b"after truncate", 0)?;
    drop(held);
    wait_pending(lab)?;
    lab.kill_client("b")?;
    lab.restore("b", Links::Authority)?;
    lab.restart_client("b")?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);

    let canaries = replacements
        .into_iter()
        .map(|(name, expected)| {
            let actual = fs::read(a.join(name))?;
            ensure!(actual == expected, "replacement canary changed");
            Ok(actual)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((fs::read(a.join(current))?, canaries))
}

/// A stationary held entry controls three entries moved twice with both old names reused. An
/// offline truncate-only close is durable but deferred; a sibling write layers over it, then an
/// immediate crash must recover only the latest bytes under the held entry id.
// contract: C1, C3, C4a, C7, C12
#[test]
fn sibling_write_over_a_deferred_truncate_survives_moves_and_a_crash() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let (control, control_canaries) = deferred_then_written(&mut lab, 0, false)?;
    assert_bytes(&control, b"after truncate", "stationary control");
    ensure!(control_canaries.is_empty(), "control has no replacements");

    for trial in 0..TRIALS {
        let (recovered, canaries) =
            deferred_then_written(&mut lab, trial, true).with_context(|| format!("moved trial {trial}"))?;
        assert_bytes(&recovered, b"after truncate", "recovered held entry");
        ensure!(canaries.len() == 2, "trial {trial}: both replacements remain");
    }
    Ok(())
}

fn rejected_files(lab: &Lab) -> Result<(u64, usize, usize)> {
    let mut bytes = 0;
    let (mut records, mut copies) = (0, 0);
    for entry in fs::read_dir(lab.client_dir("b").join("journal/rejected"))? {
        let entry = entry?;
        bytes += entry.metadata()?.len();
        match entry.path().extension().and_then(|extension| extension.to_str()) {
            Some("json") => records += 1,
            Some("copy") => copies += 1,
            _ => anyhow::bail!("unexpected rejected file {:?}", entry.path()),
        }
    }
    Ok((bytes, records, copies))
}

/// One refused sparse write controls three more refused simultaneously. Bounding by whole write
/// age must leave complete record/copy pairs and no more than the fixed journal bound.
// contract: C10
#[test]
fn concurrent_rejected_writes_are_bounded_as_whole_writes() -> Result<()> {
    const REJECTED_BYTES: u64 = 1 << 30;
    const WRITE_BYTES: u64 = 600 << 20;
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let names = (0..=TRIALS)
        .map(|index| format!("rejected-bound-{index}"))
        .collect::<Vec<_>>();
    for name in &names {
        save(&lab.mount_path("a").join(name), b"base")?;
    }
    thread::sleep(VISIBLE_AFTER);
    let files = names
        .iter()
        .map(|name| {
            OpenOptions::new()
                .write(true)
                .open(lab.mount_path("b").join(name))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    for file in &files {
        file.set_len(WRITE_BYTES)?;
    }
    lab.rpc(
        ADMIN,
        &Request::SetAcl {
            space: SPACE.into(),
            path: SpacePath::root(),
            principal: "user:bob".into(),
            perm: Some(Perm::Read),
        },
    )?;

    let mut files = files.into_iter();
    drop(files.next().context("control file")?);
    lab.wait_committed("b")?;
    let (bytes, records, copies) = rejected_files(&lab)?;
    ensure!(bytes <= REJECTED_BYTES, "control holds {bytes} rejected bytes");
    ensure!((records, copies) == (1, 1), "control keeps one complete write");

    thread::scope(|scope| {
        for file in files {
            scope.spawn(move || drop(file));
        }
    });
    lab.wait_committed("b")?;
    let (bytes, records, copies) = rejected_files(&lab)?;
    ensure!(bytes <= REJECTED_BYTES, "concurrent refusals hold {bytes} bytes");
    ensure!(records == copies, "{records} records but {copies} copies");
    ensure!(records > 0, "every rejected write was dropped");
    Ok(())
}

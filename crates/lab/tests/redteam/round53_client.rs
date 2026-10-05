//! Round 53: cold held-id materialization and reference-counted Pin eviction.

use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy};

use crate::{
    VISIBLE_AFTER, assert_bytes, random_bytes, read_at, save, stat_count, two_clients, wait_pending,
};

const TRIALS: usize = 3;

fn wait_for_fetch(lab: &Lab, before: u64, done: &AtomicBool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let fetched = stat_count(lab, "chunks_fetched")?;
        if fetched > before {
            ensure!(!done.load(Relaxed), "materialization finished before the rename");
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "materialization fetched no chunk");
        thread::sleep(Duration::from_millis(2));
    }
}

fn materialize_during_move(lab: &Lab, trial: usize, moved: bool) -> Result<Option<Vec<u8>>> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let name = format!("materialize-{moved}-{trial}");
    let relocated = format!("materialized-{trial}");
    let mut expected = random_bytes(24 << 20);
    save(&a.join(&name), &expected)?;
    thread::sleep(VISIBLE_AFTER);
    let writer = File::options().read(true).write(true).open(b.join(&name))?;
    let changed = expected.len() - 4096;
    expected[changed] ^= 0xff;
    let replacement = format!("replacement-{trial}").into_bytes();
    let before = stat_count(lab, "chunks_fetched")?;
    let done = AtomicBool::new(false);

    thread::scope(|scope| -> Result<()> {
        let done = &done;
        let write = scope.spawn(|| {
            let result = writer.write_all_at(&expected[changed..=changed], changed as u64);
            done.store(true, Relaxed);
            result
        });
        wait_for_fetch(lab, before, done)?;
        if moved {
            fs::rename(a.join(&name), a.join(&relocated))?;
            save(&a.join(&name), &replacement)?;
        }
        write.join().map_err(|_| anyhow::anyhow!("writer panicked"))??;
        Ok(())
    })?;
    writer.sync_all()?;
    drop(writer);
    thread::sleep(VISIBLE_AFTER);
    let authoritative = if moved { a.join(relocated) } else { a.join(name) };
    let decoy = moved
        .then(|| fs::read(a.join(format!("materialize-{moved}-{trial}"))))
        .transpose()?;
    let actual = fs::read(authoritative)?;
    assert_bytes(&actual, &expected, "materialized held entry");
    Ok(decoy)
}

/// A stationary cold rewrite controls three rewrites whose entry is moved and old path reused
/// while the client materializes its base. The commit must keep the held id and bytes.
// contract: C1, C3, C7
#[test]
fn cold_materialization_follows_a_held_id_during_rename_and_reuse() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Wan, Strategy::Stream)
        .cache_bytes("b", 0);
    let lab = Lab::start(config)?;

    let None = materialize_during_move(&lab, 0, false)? else {
        unreachable!("the stationary control has no replacement")
    };
    for trial in 0..TRIALS {
        let decoy = materialize_during_move(&lab, trial, true)?;
        assert_bytes(
            &decoy.context("moved trial has a replacement")?,
            format!("replacement-{trial}").as_bytes(),
            "replacement bytes",
        );
    }
    Ok(())
}

fn pin_after_closing_one(lab: &Lab, trial: usize, close_one: bool) -> Result<()> {
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    let name = format!("pin-count-{close_one}-{trial}");
    let pressure = format!("pin-pressure-{close_one}-{trial}");
    let first_version = random_bytes(6 << 20);
    save(&a.join(&name), &first_version)?;
    save(&a.join(&pressure), &random_bytes(10 << 20))?;
    thread::sleep(VISIBLE_AFTER);

    let first = File::open(b.join(&name))?;
    let remaining = File::open(b.join(&name))?;
    assert_bytes(&fs::read(b.join(&name))?, &first_version, "first pinned version");
    let latest = random_bytes(6 << 20);
    save(&a.join(&name), &latest)?;
    thread::sleep(VISIBLE_AFTER);
    ensure!(
        remaining.metadata()?.len() == latest.len() as u64,
        "switched size"
    );
    assert_bytes(
        &read_at(&remaining, 0, latest.len())?,
        &latest,
        "switched pinned version",
    );

    if close_one {
        drop(first);
    }
    fs::read(b.join(&pressure))?;
    lab.cut("b", Links::Store)?;
    let offline = read_at(&remaining, 0, latest.len());
    lab.restore("b", Links::Store)?;
    assert_bytes(&offline?, &latest, "remaining handle after eviction pressure");
    Ok(())
}

/// Keeping both Pin handles is the control. After each of three version switches, closing one
/// handle must leave the other handle's new-version chunks protected from eviction.
// contract: C3, C10, P1
#[test]
fn closing_one_of_two_pin_handles_keeps_the_other_version_pinned() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Pin)
        .cache_bytes("b", 1 << 20);
    let lab = Lab::start(config)?;
    pin_after_closing_one(&lab, 0, false)?;
    for trial in 0..TRIALS {
        pin_after_closing_one(&lab, trial, true)?;
    }
    Ok(())
}

fn recover_over_activated_truncate(lab: &mut Lab, trial: usize, crash: bool) -> Result<()> {
    let (a, b) = (lab.mount_path("a").to_owned(), lab.mount_path("b").to_owned());
    let name = format!("activated-{crash}-{trial}");
    save(&a.join(&name), b"old bytes")?;
    thread::sleep(VISIBLE_AFTER);
    let writer = File::options().read(true).write(true).open(b.join(&name))?;
    let syncer = File::open(b.join(&name))?;

    lab.cut("b", Links::Authority)?;
    drop(File::create(b.join(&name))?);
    wait_pending(lab)?;
    let (sent, synced) = mpsc::channel();
    let sync = thread::spawn(move || sent.send(syncer.sync_all()));
    thread::sleep(Duration::from_millis(300));
    writer.write_all_at(b"latest", 0)?;
    writer.set_len(6)?;
    drop(writer);
    wait_pending(lab)?;

    if crash {
        lab.kill_client("b")?;
        let _ = synced
            .recv_timeout(Duration::from_secs(10))
            .context("fsync did not return after the FUSE connection was aborted")?;
        lab.restore("b", Links::Authority)?;
        lab.restart_client("b")?;
    } else {
        lab.restore("b", Links::Authority)?;
        let _ = synced
            .recv_timeout(Duration::from_secs(10))
            .context("control fsync did not return after reconnect")?;
    }
    sync.join()
        .map_err(|_| anyhow::anyhow!("fsync thread panicked"))?
        .map_err(|_| anyhow::anyhow!("fsync result receiver went away"))?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(&fs::read(a.join(name))?, b"latest", "recovered latest seal");
    Ok(())
}

/// Reconnect is the control. In three trials a sibling fsync activates a deferred truncate, a
/// later close replaces it while offline, and an immediate crash must recover the later seal.
// contract: C1, C3, C4a, C12
#[test]
fn a_later_seal_survives_a_crash_over_an_activated_truncate() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    recover_over_activated_truncate(&mut lab, 0, false)?;
    for trial in 0..TRIALS {
        recover_over_activated_truncate(&mut lab, trial, true)?;
    }
    Ok(())
}

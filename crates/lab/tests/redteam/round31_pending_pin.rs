//! Round 31: a Pin handle opened while its client has a journal copy awaiting commit.

use std::fs::File;
use std::io::Write;

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy};

use crate::{assert_bytes, random_bytes, read_at, save};

const OLD_LEN: usize = 512 << 10;
const NEW_LEN: usize = 8 << 20;
const CACHE_BYTES: u64 = 1 << 20;
const PAGE: usize = 4096;
const TRIALS: usize = 3;

fn start() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Pin)
        .cache_bytes("b", CACHE_BYTES);
    Lab::start(config).map(Some)
}

fn reads_without_store(lab: &Lab, file: &File, bytes: &[u8]) -> Result<bool> {
    assert_bytes(&read_at(file, 0, bytes.len())?, bytes, "the committed rewrite");
    lab.cut("b", Links::Store)?;
    let reread = read_at(file, 0, PAGE);
    lab.restore("b", Links::Store)?;
    Ok(reread.is_ok_and(|page| page == bytes[..PAGE]))
}

/// A read-only open while a journal copy is pending still chooses Pin for that handle. Once the
/// copy commits, its chunks remain pinned. A fresh Pin open after the commit is each trial's
/// control.
// contract: C1, C3, P1 (Pin is chosen at open and is never evicted while its handle is open)
#[test]
#[ignore = "ROUND31: a read-only open on pending changes never chooses Pin"]
fn a_pin_handle_opened_on_pending_changes_keeps_the_committed_version_pinned() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let mut failures = 0;
    for trial in 0..TRIALS {
        let name = format!("pending-pin-{trial}");
        let path_a = lab.mount_path("a").join(&name);
        let path_b = lab.mount_path("b").join(&name);
        save(&path_a, &random_bytes(OLD_LEN))?;

        let bytes = random_bytes(NEW_LEN);
        let mut writer = File::create(&path_b)?;
        lab.cut("b", Links::Authority)?;
        writer.write_all(&bytes)?;
        drop(writer);
        ensure!(lab.stats("b")?["pending_commits"] == 1, "b's rewrite is pending");
        let held = File::open(&path_b)?;
        lab.restore("b", Links::Authority)?;
        lab.wait_committed("b")?;
        failures += usize::from(!reads_without_store(&lab, &held, &bytes)?);
        drop(held);

        let control = File::open(&path_b)?;
        ensure!(
            reads_without_store(&lab, &control, &bytes)?,
            "fresh Pin control {trial} needed the store"
        );
    }
    assert_eq!(
        failures, 0,
        "pending-change Pin handles that could not reread with the store cut, of {TRIALS}"
    );
    Ok(())
}

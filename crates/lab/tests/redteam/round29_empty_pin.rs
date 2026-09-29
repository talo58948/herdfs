//! Round 29: a Pin handle opened before its file has a first version.

use std::fs::{self, File};
use std::thread;

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy};

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, read_at, save};

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
    assert_bytes(&read_at(file, 0, bytes.len())?, bytes, "the whole first version");
    lab.cut("b", Links::Store)?;
    let reread = read_at(file, 0, PAGE);
    lab.restore("b", Links::Store)?;
    Ok(reread.is_ok_and(|page| page == bytes[..PAGE]))
}

/// A Pin handle opened on an empty entry must keep pinning when that inode gains its first version.
/// A fresh Pin open after the version exists is the control for every trial.
// contract: P1 (Pin is chosen at open and is never evicted while its handle is open)
#[test]
fn a_pin_handle_opened_before_the_first_version_keeps_that_version_pinned() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let mut failures = 0;
    for trial in 0..TRIALS {
        let name = format!("empty-pin-{trial}");
        let path_a = lab.mount_path("a").join(&name);
        let path_b = lab.mount_path("b").join(&name);
        drop(File::create(&path_a)?);
        thread::sleep(VISIBLE_AFTER);
        let held = File::open(&path_b)?;
        ensure!(held.metadata()?.len() == 0, "b opens the empty entry");

        let bytes = random_bytes(NEW_LEN);
        save(&path_a, &bytes)?;
        thread::sleep(VISIBLE_AFTER);
        ensure!(fs::metadata(&path_b)?.len() == NEW_LEN as u64, "b follows v1");
        failures += usize::from(!reads_without_store(&lab, &held, &bytes)?);
        drop(held);

        let control = File::open(&path_b)?;
        ensure!(
            reads_without_store(&lab, &control, &bytes)?,
            "fresh Pin control {trial} needed the store"
        );
    }
    assert_eq!(failures, 0, "unpinning first-version handles, of {TRIALS}");
    Ok(())
}

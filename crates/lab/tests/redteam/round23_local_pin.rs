//! Round 23: a Pin handle after its own client commits a newer version.

use std::fs::{self, File};
use std::thread;

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy};

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, read_at, save};

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

/// Fills b's cache from `file`, cuts its store link, and reports whether the first page remains.
fn reads_without_store(lab: &Lab, file: &File, bytes: &[u8]) -> Result<bool> {
    assert_bytes(&read_at(file, 0, bytes.len())?, bytes, "the whole new version");
    lab.cut("b", Links::Store)?;
    let reread = read_at(file, 0, PAGE);
    lab.restore("b", Links::Store)?;
    Ok(reread.is_ok_and(|page| page == bytes[..PAGE]))
}

/// b opens v1 with Pin and then commits v2 through another handle. The held handle follows v2;
/// cache pressure tests that v2 replaced v1 in its pin set. A fresh Pin open of v2 is the control.
// contract: C1, C3, P1 (docs/architecture.md: Pin is never evicted while pinned)
#[test]
fn a_pin_handle_keeps_a_new_version_committed_by_its_client_pinned() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let mut switched_failures = 0;
    for trial in 0..TRIALS {
        let name = format!("local-pin-{trial}");
        let path_a = lab.mount_path("a").join(&name);
        let path_b = lab.mount_path("b").join(&name);
        save(&path_a, &random_bytes(OLD_LEN))?;
        thread::sleep(VISIBLE_AFTER);
        let held = File::open(&path_b)?;
        let new = random_bytes(NEW_LEN);
        save(&path_b, &new)?;
        ensure!(fs::metadata(&path_b)?.len() == NEW_LEN as u64, "b follows v2");
        switched_failures += usize::from(!reads_without_store(&lab, &held, &new)?);
        drop(held);

        let control = File::open(&path_b)?;
        ensure!(
            reads_without_store(&lab, &control, &new)?,
            "fresh Pin control {trial} needed the store"
        );
    }
    assert_eq!(
        switched_failures, 0,
        "locally switched Pin handles that could not reread with the store cut, of {TRIALS}"
    );
    Ok(())
}

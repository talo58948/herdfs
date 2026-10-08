//! Round 29: a Pin handle opened before its file has a first version.

use std::fs::{self, File};
use std::thread;

use anyhow::{Result, ensure};

use crate::{PIN_NEW_LEN, PIN_TRIALS, VISIBLE_AFTER, pin_lab, random_bytes, save, stays_pinned};

/// A Pin handle opened on an empty entry must keep pinning when that inode gains its first version.
/// A fresh Pin open after the version exists is the control for every trial.
// contract: P1 (Pin is chosen at open and is never evicted while its handle is open)
#[test]
fn a_pin_handle_opened_before_the_first_version_keeps_that_version_pinned() -> Result<()> {
    let Some(lab) = pin_lab()? else {
        return Ok(());
    };
    let mut failures = 0;
    for trial in 0..PIN_TRIALS {
        let name = format!("empty-pin-{trial}");
        let path_a = lab.mount_path("a").join(&name);
        let path_b = lab.mount_path("b").join(&name);
        drop(File::create(&path_a)?);
        thread::sleep(VISIBLE_AFTER);
        let held = File::open(&path_b)?;
        ensure!(held.metadata()?.len() == 0, "b opens the empty entry");

        let bytes = random_bytes(PIN_NEW_LEN);
        save(&path_a, &bytes)?;
        thread::sleep(VISIBLE_AFTER);
        ensure!(fs::metadata(&path_b)?.len() == PIN_NEW_LEN as u64, "b follows v1");
        failures += usize::from(!stays_pinned(&lab, &held, &bytes, "the whole first version")?);
        drop(held);

        let control = File::open(&path_b)?;
        ensure!(
            stays_pinned(&lab, &control, &bytes, "fresh Pin version")?,
            "fresh Pin control {trial} needed the store"
        );
    }
    assert_eq!(failures, 0, "unpinning first-version handles, of {PIN_TRIALS}");
    Ok(())
}

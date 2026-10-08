//! Round 31: a Pin handle opened while its client has a journal copy awaiting commit.

use std::fs::File;
use std::io::Write;

use anyhow::{Result, ensure};
use herdfs_lab::Links;

use crate::{PIN_NEW_LEN, PIN_OLD_LEN, PIN_TRIALS, pin_lab, random_bytes, save, stays_pinned};

/// A read-only open while a journal copy is pending still chooses Pin for that handle. Once the
/// copy commits, its chunks remain pinned. A fresh Pin open after the commit is each trial's
/// control.
// contract: C1, C3, P1 (Pin is chosen at open and is never evicted while its handle is open)
#[test]
fn a_pin_handle_opened_on_pending_changes_keeps_the_committed_version_pinned() -> Result<()> {
    let Some(lab) = pin_lab()? else {
        return Ok(());
    };
    let mut failures = 0;
    for trial in 0..PIN_TRIALS {
        let name = format!("pending-pin-{trial}");
        let path_a = lab.mount_path("a").join(&name);
        let path_b = lab.mount_path("b").join(&name);
        save(&path_a, &random_bytes(PIN_OLD_LEN))?;

        let bytes = random_bytes(PIN_NEW_LEN);
        let mut writer = File::create(&path_b)?;
        lab.cut("b", Links::Authority)?;
        writer.write_all(&bytes)?;
        drop(writer);
        ensure!(lab.stats("b")?["pending_commits"] == 1, "b's rewrite is pending");
        let held = File::open(&path_b)?;
        lab.restore("b", Links::Authority)?;
        lab.wait_committed("b")?;
        failures += usize::from(!stays_pinned(&lab, &held, &bytes, "the committed rewrite")?);
        drop(held);

        let control = File::open(&path_b)?;
        ensure!(
            stays_pinned(&lab, &control, &bytes, "fresh Pin version")?,
            "fresh Pin control {trial} needed the store"
        );
    }
    assert_eq!(
        failures, 0,
        "pending-change Pin handles that could not reread with the store cut, of {PIN_TRIALS}"
    );
    Ok(())
}

//! Round 23: a Pin handle after its own client commits a newer version.

use std::fs::{self, File};
use std::thread;

use anyhow::{Result, ensure};

use crate::{PIN_NEW_LEN, PIN_OLD_LEN, PIN_TRIALS, VISIBLE_AFTER, pin_lab, random_bytes, save, stays_pinned};

/// b opens v1 with Pin and then commits v2 through another handle. The held handle follows v2;
/// cache pressure tests that v2 replaced v1 in its pin set. A fresh Pin open of v2 is the control.
// contract: C1, C3, P1 (docs/architecture.md: Pin is never evicted while pinned)
#[test]
fn a_pin_handle_keeps_a_new_version_committed_by_its_client_pinned() -> Result<()> {
    let Some(lab) = pin_lab()? else {
        return Ok(());
    };
    let mut switched_failures = 0;
    for trial in 0..PIN_TRIALS {
        let name = format!("local-pin-{trial}");
        let path_a = lab.mount_path("a").join(&name);
        let path_b = lab.mount_path("b").join(&name);
        save(&path_a, &random_bytes(PIN_OLD_LEN))?;
        thread::sleep(VISIBLE_AFTER);
        let held = File::open(&path_b)?;
        let new = random_bytes(PIN_NEW_LEN);
        save(&path_b, &new)?;
        ensure!(fs::metadata(&path_b)?.len() == PIN_NEW_LEN as u64, "b follows v2");
        switched_failures += usize::from(!stays_pinned(&lab, &held, &new, "the whole new version")?);
        drop(held);

        let control = File::open(&path_b)?;
        ensure!(
            stays_pinned(&lab, &control, &new, "fresh Pin version")?,
            "fresh Pin control {trial} needed the store"
        );
    }
    assert_eq!(
        switched_failures, 0,
        "locally switched Pin handles that could not reread with the store cut, of {PIN_TRIALS}"
    );
    Ok(())
}

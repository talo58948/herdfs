//! Round 21: a Pin handle after its inode follows a newer version.

use std::fs::{self, File};
use std::thread;

use anyhow::{Result, ensure};

use crate::{PIN_NEW_LEN, PIN_OLD_LEN, PIN_TRIALS, VISIBLE_AFTER, pin_lab, random_bytes, save, stays_pinned};

/// Repro: b opens v1 with Pin. After a commits v2, a stat switches the held inode to v2. Reading
/// all of v2 exceeds b's cache; with the store cut, rereading its start tests whether the Pin
/// chosen at open was retained across that switch. A fresh Pin open of v2 is the control.
// contract: C3, P1 (docs/architecture.md: Pin is chosen once per open and is never evicted while pinned)
#[test]
fn a_pin_handle_keeps_the_version_it_follows_pinned() -> Result<()> {
    let Some(lab) = pin_lab()? else {
        return Ok(());
    };
    let mut switched_failures = 0;
    for trial in 0..PIN_TRIALS {
        let name = format!("pin-{trial}");
        let path_a = lab.mount_path("a").join(&name);
        let path_b = lab.mount_path("b").join(&name);
        save(&path_a, &random_bytes(PIN_OLD_LEN))?;
        thread::sleep(VISIBLE_AFTER);
        let held = File::open(&path_b)?;
        let new = random_bytes(PIN_NEW_LEN);
        save(&path_a, &new)?;
        thread::sleep(VISIBLE_AFTER);
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
        "switched Pin handles that could not reread with the store cut, of {PIN_TRIALS}"
    );
    Ok(())
}

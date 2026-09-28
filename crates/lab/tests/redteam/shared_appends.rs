//! Round 16 probe: an append on one client to a file another client extended, through an open
//! made T_vis after that client's commit.

use std::fs::{self, File};
use std::io::Write;
use std::thread;

use anyhow::Result;

use crate::{VISIBLE_AFTER, save, two_clients};

const TRIALS: usize = 3;

/// In each trial, a saves `0123456789`; b reads it; a appends `ABCDE` and fsyncs; after T_vis b
/// opens the file with O_APPEND, appends `X` and fsyncs; after T_vis a reads it back.
fn append_after_another_append() -> Result<Option<Vec<String>>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let mut seen = Vec::new();
    for trial in 0..TRIALS {
        let name = format!("log{trial}");
        let (on_a, on_b) = (lab.mount_path("a").join(&name), lab.mount_path("b").join(&name));
        save(&on_a, b"0123456789")?;
        thread::sleep(VISIBLE_AFTER);
        fs::read(&on_b)?;
        let mut log = File::options().append(true).open(&on_a)?;
        log.write_all(b"ABCDE")?;
        log.sync_all()?;
        drop(log);
        thread::sleep(VISIBLE_AFTER);
        let mut log = File::options().append(true).open(&on_b)?;
        log.write_all(b"X")?;
        log.sync_all()?;
        drop(log);
        thread::sleep(VISIBLE_AFTER);
        seen.push(String::from_utf8_lossy(&fs::read(&on_a)?).into_owned());
    }
    Ok(Some(seen))
}

/// An open made T_vis after the other client's commit sees its size, so an O_APPEND write
/// lands after its bytes rather than over them.
// contract: C2
#[test]
fn an_append_after_another_clients_append_lands_at_the_end() -> Result<()> {
    let Some(seen) = append_after_another_append()? else {
        return Ok(());
    };
    assert_eq!(seen, ["0123456789ABCDEX"; TRIALS]);
    Ok(())
}

//! Round 19: atomic saves (write a temporary file, fsync, rename it over the target) while the
//! authority is SIGKILLed and restarted, as F2 does for plain writes.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};

use crate::{VISIBLE_AFTER, random_bytes, two_clients};

const TARGET: &str = "target";
/// Authority crashes.
const KILLS: usize = 6;

/// One save: its bytes, and whether its rename over the target returned success.
struct Save {
    bytes: Vec<u8>,
    renamed: bool,
}

/// Saves new bytes over [`TARGET`] on `mount`, each through a synced temporary file, until `stop`.
fn save_over(mount: &Path, stop: &AtomicBool) -> Result<Vec<Save>> {
    let mut saves = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let temporary = mount.join(format!("tmp-{}", saves.len()));
        let bytes = random_bytes(rand::random_range(64 << 10..512 << 10));
        // Creating a file needs the authority; while it is down, try again.
        let Ok(mut file) = File::create(&temporary) else {
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        file.write_all(&bytes)?;
        let _ = file.sync_all();
        drop(file);
        let renamed = fs::rename(&temporary, mount.join(TARGET)).is_ok();
        saves.push(Save { bytes, renamed });
    }
    Ok(saves)
}

/// a saves over the target again and again while the authority crashes [`KILLS`] times. Once a
/// has nothing left to commit, both clients read the target whole as the last save whose rename
/// succeeded, or as a later one whose rename failed after the authority applied it (C7, C4b).
// contract: C7, C4b, C3
#[test]
fn atomic_saves_survive_authority_crashes_around_them() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let stop = Arc::new(AtomicBool::new(false));
    let saver = {
        let (mount, stop) = (lab.mount_path("a").to_owned(), stop.clone());
        thread::spawn(move || save_over(&mount, &stop))
    };
    for _ in 0..KILLS {
        thread::sleep(Duration::from_millis(rand::random_range(200..800)));
        lab.kill_authority();
        thread::sleep(Duration::from_millis(rand::random_range(50..300)));
        lab.restart_authority()?;
    }
    stop.store(true, Ordering::Relaxed);
    let saves = saver.join().map_err(|_| anyhow!("the saver panicked"))??;
    let last = saves.iter().rposition(|save| save.renamed);
    let last = last.ok_or_else(|| anyhow!("no rename of {} succeeded", saves.len()))?;
    let refused = saves.iter().filter(|save| !save.renamed).count();
    let count = saves.len();
    eprintln!("{count} saves, {refused} renames failed, the last renamed is #{last}");
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    for client in ["a", "b"] {
        let read = fs::read(lab.mount_path(client).join(TARGET))?;
        let found = saves[last..].iter().position(|save| save.bytes == read);
        ensure!(
            found.is_some(),
            "{client} reads {} bytes at {TARGET}, none of saves #{last}.. ",
            read.len()
        );
    }
    Ok(())
}

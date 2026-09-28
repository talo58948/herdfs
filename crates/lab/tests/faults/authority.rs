//! F2: SIGKILL the authority while two clients commit.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};

use crate::{VISIBLE_AFTER, assert_history, assert_reads, random_bytes, two_clients, wait_committed};

/// The clients of `two_clients`, both writing.
const CLIENTS: [&str; 2] = ["a", "b"];
/// Authority crashes.
const KILLS: usize = 8;

/// A file a writer closed, and whether its fsync saw the commit.
struct Written {
    name: String,
    bytes: Vec<u8>,
    acknowledged: bool,
}

/// Writes new files on `mount`, one at a time and each synced, until `stop`.
fn write_files(mount: &Path, prefix: &str, stop: &AtomicBool) -> Result<Vec<Written>> {
    let mut written = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let name = format!("{prefix}-{}.bin", written.len());
        let bytes = random_bytes(rand::random_range(64 << 10..512 << 10));
        // Creating a file needs the authority; while it is down, try again.
        let Ok(mut file) = File::create(mount.join(&name)) else {
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        file.write_all(&bytes)?;
        let acknowledged = file.sync_all().is_ok();
        written.push(Written {
            name,
            bytes,
            acknowledged,
        });
    }
    Ok(written)
}

// contract: C4b, C3
#[test]
fn f2_commits_survive_authority_crashes_around_them() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let stop = Arc::new(AtomicBool::new(false));
    let writers = CLIENTS.map(|client| {
        let (mount, stop) = (lab.mount_path(client).to_owned(), stop.clone());
        thread::spawn(move || write_files(&mount, client, &stop))
    });
    for _ in 0..KILLS {
        thread::sleep(Duration::from_millis(rand::random_range(200..800)));
        lab.kill_authority();
        thread::sleep(Duration::from_millis(rand::random_range(50..300)));
        lab.restart_authority()?;
    }
    stop.store(true, Ordering::Relaxed);
    let mut written = Vec::new();
    for (client, writer) in CLIENTS.into_iter().zip(writers) {
        let joined = writer.join();
        let files = joined.map_err(|_| anyhow!("{client}'s writer panicked"))??;
        written.extend(files.into_iter().map(|file| (client, file)));
    }
    let acknowledged = written.iter().filter(|(_, file)| file.acknowledged).count();
    eprintln!("{} files closed, {acknowledged} acknowledged", written.len());
    ensure!(acknowledged > 0, "no commit was acknowledged");
    for client in CLIENTS {
        wait_committed(&lab, client)?;
    }
    thread::sleep(VISIBLE_AFTER);
    // Every closed file ends as one whole version, readable on the other client; for the
    // acknowledged ones that is C4b, for the rest C4a.
    for (client, file) in &written {
        assert_history(&lab, &file.name, &[&file.bytes])?;
        let other = if *client == CLIENTS[0] {
            CLIENTS[1]
        } else {
            CLIENTS[0]
        };
        assert_reads(&lab, other, &file.name, &file.bytes)?;
    }
    Ok(())
}

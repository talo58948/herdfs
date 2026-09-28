//! Round 19: two clients rewrite one file, each write synced, while the authority is SIGKILLed
//! and restarted, as F2 does for separate files.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow, bail, ensure};
use herdfs_core::proto::{Request, Response};
use herdfs_core::{ChunkRef, SpacePath, chunk};
use herdfs_lab::{ADMIN, SPACE};

use crate::{VISIBLE_AFTER, random_bytes, two_clients};

const NAME: &str = "shared";
const CLIENTS: [&str; 2] = ["a", "b"];
/// Authority crashes.
const KILLS: usize = 6;

/// One rewrite: its chunks, and whether its fsync saw the commit.
struct Rewrite {
    chunks: Vec<ChunkRef>,
    acknowledged: bool,
}

/// Rewrites [`NAME`] on `mount` with new bytes, each synced, until `stop`.
fn rewrite(mount: &Path, stop: &AtomicBool) -> Result<Vec<Rewrite>> {
    let mut rewrites = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let bytes = random_bytes(rand::random_range(64 << 10..512 << 10));
        let Ok(mut file) = File::create(mount.join(NAME)) else {
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        file.write_all(&bytes)?;
        let acknowledged = file.sync_all().is_ok();
        let chunks = chunk::split(&bytes[..]).map(|chunk| chunk.map(|(chunk, _)| chunk));
        rewrites.push(Rewrite {
            chunks: chunks.collect::<std::io::Result<_>>()?,
            acknowledged,
        });
    }
    Ok(rewrites)
}

/// The chunk lists of [`NAME`]'s versions, oldest first.
fn history(lab: &herdfs_lab::Lab) -> Result<Vec<Vec<ChunkRef>>> {
    let request = Request::History {
        space: SPACE.into(),
        path: SpacePath::try_from(NAME)?,
    };
    let Response::History { versions } = lab.rpc(ADMIN, &request)? else {
        bail!("history answers with versions");
    };
    Ok(versions.into_iter().map(|version| version.chunks).collect())
}

/// a and b rewrite one file while the authority crashes [`KILLS`] times. Once both have nothing
/// left to commit, every acknowledged rewrite is in the history exactly once (C4b, C5), every
/// version is one whole rewrite (C3), and both clients read the head.
// contract: C4b, C5, C3
#[test]
fn rewrites_of_one_file_survive_authority_crashes_around_them() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let seed = random_bytes(1 << 10);
    fs::write(lab.mount_path("a").join(NAME), &seed)?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    let stop = Arc::new(AtomicBool::new(false));
    let writers = CLIENTS.map(|client| {
        let (mount, stop) = (lab.mount_path(client).to_owned(), stop.clone());
        thread::spawn(move || rewrite(&mount, &stop))
    });
    for _ in 0..KILLS {
        thread::sleep(Duration::from_millis(rand::random_range(200..800)));
        lab.kill_authority();
        thread::sleep(Duration::from_millis(rand::random_range(50..300)));
        lab.restart_authority()?;
    }
    stop.store(true, Ordering::Relaxed);
    let mut rewrites = Vec::new();
    for writer in writers {
        rewrites.extend(writer.join().map_err(|_| anyhow!("a writer panicked"))??);
    }
    for client in CLIENTS {
        lab.wait_committed(client)?;
    }
    thread::sleep(VISIBLE_AFTER);
    let versions = history(&lab)?;
    let acknowledged = rewrites.iter().filter(|rewrite| rewrite.acknowledged).count();
    eprintln!(
        "{} rewrites, {acknowledged} acknowledged, {} versions",
        rewrites.len(),
        versions.len()
    );
    for (at, rewrite) in rewrites.iter().enumerate() {
        let times = versions
            .iter()
            .filter(|version| **version == rewrite.chunks)
            .count();
        ensure!(times <= 1, "rewrite #{at} is {times} versions");
        ensure!(
            times == 1 || !rewrite.acknowledged,
            "acknowledged rewrite #{at} is no version"
        );
    }
    let seeded = chunk::split(&seed[..]).map(|chunk| chunk.map(|(chunk, _)| chunk));
    let seeded: Vec<ChunkRef> = seeded.collect::<std::io::Result<_>>()?;
    for (at, version) in versions.iter().enumerate() {
        let whole = *version == seeded || rewrites.iter().any(|rewrite| rewrite.chunks == *version);
        ensure!(whole, "version #{at} is no rewrite");
    }
    let head = versions.last().ok_or_else(|| anyhow!("no versions"))?;
    for client in CLIENTS {
        let read = fs::read(lab.mount_path(client).join(NAME))?;
        let chunks = chunk::split(&read[..]).map(|chunk| chunk.map(|(chunk, _)| chunk));
        let chunks: Vec<ChunkRef> = chunks.collect::<std::io::Result<_>>()?;
        ensure!(
            chunks == *head,
            "{client} reads {} bytes, not the head",
            read.len()
        );
    }
    Ok(())
}

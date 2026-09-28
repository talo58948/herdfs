//! Round 3: files within the documented size limit, opened over the lab's slowest link.

use std::fs::File;
use std::io::ErrorKind;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use herdfs_core::proto::Request;
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, SPACE, Strategy};

use crate::{VISIBLE_AFTER, random_bytes, save};

/// Chunks in `f`'s version. Their list, about 87 bytes each in JSON, takes the Bad link about
/// 5 s to carry; the RPC body limit allows about 350,000.
const CHUNKS: usize = 8_000;
/// One chunk: files below the 256 KiB minimum chunk are a single chunk.
const CHUNK: usize = 256 << 10;
/// Opens b tries, each after the offline window a failed one opened has ended.
const TRIES: usize = 3;
const WINDOW: Duration = Duration::from_secs(3);

/// Repro: a saves `f`, one 256 KiB chunk; the admin commits a version of `f` that repeats that
/// chunk 8,000 times, a 2 GiB file (a file written through a mount has as many chunks at 2 to 8
/// GiB); b, on the lab's Bad link (1 Mbit/s), opens `f`. The `Open` reply lists every chunk,
/// about 700 KB, which b's link carries in about 5 s, but the client gives every authority call
/// 3 s in all, including the reply's body (remote.rs `RPC_TIMEOUT`): each open fails with EIO
/// and opens the offline window. a, on LAN, opens it at once.
// contract: C2 (docs/architecture.md Limits: largest file about 350 GiB)
#[test]
fn a_file_of_many_chunks_opens_over_a_slow_link() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Bad, Strategy::Stream);
    let lab = Lab::start(config)?;
    save(&lab.mount_path("a").join("f"), &random_bytes(CHUNK))?;
    let first = lab.head(ADMIN, "f")?;
    let commit = Request::Commit {
        space: SPACE.into(),
        entry: first.entry,
        request_id: "many-chunks".into(),
        base: Some(first.id),
        guarded: false,
        chunks: vec![first.chunks[0]; CHUNKS],
        mtime_ms: None,
    };
    lab.rpc(ADMIN, &commit)?;
    thread::sleep(VISIBLE_AFTER);
    File::open(lab.mount_path("a").join("f"))?;

    let path = lab.mount_path("b").join("f");
    let mut failures = Vec::new();
    for _ in 0..TRIES {
        match File::open(&path) {
            Ok(_) => break,
            Err(error) if error.kind() != ErrorKind::NotFound => failures.push(error.to_string()),
            Err(error) => return Err(error.into()),
        }
        thread::sleep(WINDOW);
    }
    assert!(
        failures.len() < TRIES,
        "b's opens of a {} MiB file of {CHUNKS} chunks: {failures:?}",
        (CHUNKS * CHUNK) >> 20
    );
    Ok(())
}

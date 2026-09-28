//! Round 5: a client's local disk after files that are written and removed at once, as build
//! tools do with their scratch files (C10).

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Result;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

use crate::random_bytes;

const CACHE_BYTES: u64 = 16 << 20;
const FILE_LEN: usize = 8 << 20;
const FILES: usize = 20;
/// Logs, the token and filesystem block rounding on top of the cache, as U8 allows.
const SLACK: u64 = 1 << 20;

/// Repro: with a 16 MiB chunk cache, a writes an 8 MiB scratch file and removes it right after
/// closing it, 20 times, as a compiler's temporary files, make's intermediate files and rustc's
/// codegen units are. Each close leaves a commit waiting out 250 ms (journal.rs `QUIET`); the
/// remove reaches the authority first, the commit then finds no entry, and the committer takes
/// that refusal as final and keeps the write, with its bytes, in `journal/rejected/`
/// (commit.rs `reject`), where nothing ever removes it. The mount's disk grows by every byte
/// written and removed, 160 MiB here, ten times its cache. Removing each file only once it has
/// committed leaves nothing behind.
// contract: C10
#[test]
#[ignore = "REDTEAM5: files removed before they commit keep their bytes on the client's disk for good"]
fn files_removed_before_they_commit_leave_no_disk_behind() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .cache_bytes("a", CACHE_BYTES);
    let lab = Lab::start(config)?;
    let bytes = random_bytes(FILE_LEN);
    for n in 0..FILES {
        let scratch = lab.mount_path("a").join(format!("scratch{n}.o"));
        fs::write(&scratch, &bytes)?;
        fs::remove_file(&scratch)?;
    }
    lab.wait_committed("a")?;
    let used = disk_use(lab.client_dir("a"))?;
    assert!(
        used <= CACHE_BYTES + SLACK,
        "a's disk holds {used} bytes after {} bytes written and removed, with a {CACHE_BYTES}-byte cache",
        FILE_LEN * FILES
    );
    Ok(())
}

fn disk_use(dir: &Path) -> Result<u64> {
    let mut used = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        used += if meta.is_dir() {
            disk_use(&entry.path())?
        } else {
            meta.blocks() * 512
        };
    }
    Ok(used)
}

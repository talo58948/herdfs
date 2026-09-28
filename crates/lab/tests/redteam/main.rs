//! Red team: confirmed contract breaks, each reduced to one failing test. Rounds 1 to 4 are fixed
//! and run. Skips until the binaries are built.

mod agents;
mod commit_manifest;
mod creates;
mod debounce;
mod followed;
mod followed_paths;
mod journal;
mod mtime;
mod offline;
mod offline_heads;
mod offline_writes;
mod open_handles;
mod paths;
mod recovery;
mod slow_links;
mod versions;

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

/// T_vis plus a margin for the test's own scheduling.
const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);

/// Client `a` as alice and client `b` as bob, both streaming over LAN links, or `None` until the
/// binaries are built.
fn two_clients() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream);
    Lab::start(config).map(Some)
}

/// Writes `bytes` to `path` and returns once the authority has committed them: fsync waits.
fn save(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn read_at(file: &File, off: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; len];
    file.read_exact_at(&mut bytes, off)?;
    Ok(bytes)
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    rand::fill(&mut bytes[..]);
    bytes
}

/// Fails unless `got` is exactly `expected`, naming the first differing offset instead of
/// printing both.
fn assert_bytes(got: &[u8], expected: &[u8], what: &str) {
    let differs = got
        .iter()
        .zip(expected)
        .position(|(got, expected)| got != expected);
    assert!(
        got == expected,
        "{what}: {} bytes, {} expected; first difference at {differs:?}",
        got.len(),
        expected.len()
    );
}

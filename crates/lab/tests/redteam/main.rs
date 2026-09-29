//! Red team: confirmed contract breaks, each reduced to one failing test. Rounds 1 to 7 are fixed
//! and run; round 7's are ignored until fixed. Skips until the binaries are built.

mod agents;
mod appends;
mod atomic_saves;
mod build_mtimes;
mod collection_reads;
mod commit_manifest;
mod crashed_rewrites;
mod crashed_saves;
mod creates;
mod crowded_writes;
mod debounce;
mod followed;
mod followed_paths;
mod forgotten_replays;
mod journal;
mod listed_heads;
mod listed_renames;
mod lock_files;
mod locks;
mod mapped;
mod moved_removes;
mod mtime;
mod offline;
mod offline_heads;
mod offline_reads;
mod offline_renames;
mod offline_writes;
mod open_handles;
mod paths;
mod pending_opens;
mod pin_switches;
mod raced_renames;
mod recovery;
mod refused_fsync;
mod rejected_writes;
mod remapped;
mod replay_answers;
mod replayed_commits;
mod replayed_heads;
mod revoked_replays;
mod rewound_reads;
mod round21_rename_race;
mod round22_agents;
mod round23_local_pin;
mod round23_rename_settle;
mod round23_reused_parent;
mod round25_moving_parent;
mod saved_mtime;
mod scratch_files;
mod shared_appends;
mod shared_builds;
mod slow_links;
mod slow_uploads;
mod stalled_reads;
mod stalled_stores;
mod synced_opens;
mod times;
mod unstored_uploads;
mod unusable_versions;
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

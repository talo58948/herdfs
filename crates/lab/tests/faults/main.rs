//! Faults: crashes (F1, F2), cut links (F3, F4), a bad link (F5), a full cache (F7) and Jev slow
//! or down (F8). Each test asserts the contract lines that must hold through its fault. The oracle
//! for bytes is what the test wrote, and for versions the chunk lists `chunk::split` gives for
//! those bytes.
//! Skips until the binaries are built.

mod authority;
mod crash;
mod jev;
mod links;
mod reads;

use std::fs::{self, File};
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use herdfs_core::proto::{Request, Response};
use herdfs_core::{ChunkRef, SpacePath, chunk};
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, SPACE, Strategy};

/// T_vis plus a margin for the test's own scheduling.
const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);
/// A committer backs off to 30 s between attempts, so a write lands within this of its link
/// returning.
const COMMIT_WITHIN: Duration = Duration::from_secs(45);
/// A 32 MiB commit has taken 52 s under contention on the 2-core CI runner.
const LARGE_COMMIT_WITHIN: Duration = Duration::from_secs(90);

fn start(config: LabConfig) -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    Lab::start(config).map(Some)
}

/// Client `a` as alice and client `b` as bob, both streaming over LAN links.
fn two_clients() -> Result<Option<Lab>> {
    start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    rand::fill(&mut bytes[..]);
    bytes
}

/// Writes `bytes` to `name` through client `client` and returns once they are committed.
fn commit(lab: &Lab, client: &str, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(lab.mount_path(client).join(name))?;
    file.write_all(bytes)?;
    // fsync returns once the authority has committed the version.
    file.sync_all()?;
    Ok(())
}

/// The chunks a commit of `bytes` consists of.
fn chunks_of(bytes: &[u8]) -> Result<Vec<ChunkRef>> {
    let chunks = chunk::split(bytes).map(|chunk| chunk.map(|(chunk, _)| chunk));
    Ok(chunks.collect::<std::io::Result<_>>()?)
}

/// The chunk lists of `name`'s versions, oldest first.
fn history(lab: &Lab, name: &str) -> Result<Vec<Vec<ChunkRef>>> {
    let request = Request::History {
        space: SPACE.into(),
        path: SpacePath::try_from(name)?,
    };
    let Response::History { versions } = lab.rpc(ADMIN, &request)? else {
        bail!("history answers with versions");
    };
    Ok(versions.into_iter().map(|version| version.chunks).collect())
}

/// `name`'s history must be exactly `contents`, in order: each write one whole version.
fn assert_history(lab: &Lab, name: &str, contents: &[&[u8]]) -> Result<()> {
    let expected = contents
        .iter()
        .map(|bytes| chunks_of(bytes))
        .collect::<Result<Vec<_>>>()?;
    let found = history(lab, name)?;
    let sizes = |versions: &[Vec<ChunkRef>]| -> Vec<u64> {
        versions.iter().map(|chunks| chunk::total_len(chunks)).collect()
    };
    assert!(
        found == expected,
        "{name}: versions of {:?} bytes, expected {:?}",
        sizes(&found),
        sizes(&expected)
    );
    Ok(())
}

/// Client `client` reads `name` as exactly `bytes`.
fn assert_reads(lab: &Lab, client: &str, name: &str, bytes: &[u8]) -> Result<()> {
    let read = fs::read(lab.mount_path(client).join(name))?;
    let (got, expected) = (read.len(), bytes.len());
    assert!(
        read == bytes,
        "{client} reads {got} bytes of {name}, not the {expected} written"
    );
    Ok(())
}

/// Polls `done` until it holds, failing after `timeout`.
fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !done()? {
        if Instant::now() > deadline {
            bail!("{what}: not after {timeout:?}");
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Waits until client `client` has committed every write it closed.
fn wait_committed(lab: &Lab, client: &str) -> Result<()> {
    wait_committed_within(lab, client, COMMIT_WITHIN)
}

fn wait_committed_within(lab: &Lab, client: &str, within: Duration) -> Result<()> {
    wait_until(&format!("{client} commits its writes"), within, || {
        Ok(lab.stats(client)?["pending_commits"] == 0)
    })
}

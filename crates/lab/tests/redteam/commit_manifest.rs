//! Round 4: committing a file whose chunk-list RPC body outlasts the 3 s deadline.

use std::fs::File;
use std::io::{self, Write};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

/// One block, repeated to make a file of about this many content-defined chunks. FastCDC cuts a
/// 1 MiB-periodic stream at each period, so the head version lists about this many refs; every
/// repeat is the same chunk, so only a few distinct chunks are ever stored (~3).
const BLOCK: usize = 1 << 20;
const REPS: usize = 256;
/// b's link: 4 KiB/s. The head's ~256-chunk list is about 22 KiB of JSON, which takes ~5 s to
/// send, past the client's 3 s RPC deadline (remote.rs `RPC_TIMEOUT`). A one-chunk file's list
/// is ~90 bytes and commits at once, so the link itself is not the obstacle.
const KIBS: u32 = 4;
/// How long to watch b's pending commit before calling it stuck: several 3 s deadlines and
/// back-offs, far longer than the ~5 s a single send would need.
const WATCH: Duration = Duration::from_secs(25);

/// A deterministic block, so a and b chunk the same bytes into the same ids. This particular
/// stream makes FastCDC cut at each 1 MiB period, so REPS copies become about REPS chunks.
fn block() -> Vec<u8> {
    let mut state = 0x1234u64;
    (0..BLOCK)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            (state >> 56) as u8
        })
        .collect()
}

/// Writes `reps` copies of `block` to a fresh file at `path`, then closes it without fsync, as a
/// program that writes a large file and moves on does. Close returns once the write is journaled.
fn write_reps(path: &std::path::Path, block: &[u8], reps: usize) -> io::Result<()> {
    let mut file = File::create(path)?;
    for _ in 0..reps {
        file.write_all(block)?;
    }
    Ok(())
}

/// Polls b's pending commit count until it clears or `deadline` passes; returns whether it cleared.
fn committed_within(lab: &Lab, name: &str, deadline: Duration) -> Result<bool> {
    let until = Instant::now() + deadline;
    loop {
        if lab.stats(name)?["pending_commits"] == 0 {
            return Ok(true);
        }
        if Instant::now() >= until {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Repro: alice, on a LAN link, saves `big`, a 256 MiB file of a repeated 1 MiB block, so its head
/// version lists ~256 chunks but stores only ~3. bob, on a 4 KiB/s link, first saves `small`, a
/// one-chunk file, which commits: the slow link commits small chunk lists fine. bob then opens
/// `big` with O_TRUNC and rewrites the identical bytes and closes; every chunk is already in the
/// base, so nothing uploads, and the only remaining work is one `Commit` whose ~22 KiB chunk list
/// takes ~5 s to send. The client wraps the whole call, request body included, in a 3 s deadline
/// (remote.rs `call`/`send`, `RPC_TIMEOUT`); the send never finishes in time, so the commit times
/// out, opens an offline window, and retries forever. bob's close returned, but the write never
/// commits. Round 3 moved the reply's body out of that deadline, but not the request's.
// contract: C4a (docs/architecture.md Write: a background committer commits the copy; Limits: largest file about 350 GiB)
#[test]
fn a_commit_with_a_large_chunk_list_lands_on_a_slow_link() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let slow = Profile::Custom {
        round_trip_ms: 30,
        kilobytes_per_sec: KIBS,
        store_first_byte_ms: 0,
    };
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], slow, Strategy::Stream);
    let lab = Lab::start(config)?;
    let block = block();

    // alice, on the fast link, publishes the many-chunk file.
    write_reps(&lab.mount_path("a").join("big"), &block, REPS)?;
    lab.wait_committed("a")?;

    // Control: bob commits a tiny, one-chunk file over the same slow link. Its bytes and its
    // ~90-byte chunk list both cross in well under a second, so it commits at once: the slow link
    // is not the obstacle, a large chunk list is.
    File::create(lab.mount_path("b").join("small"))?.write_all(b"tiny\n")?;
    assert!(
        committed_within(&lab, "b", Duration::from_secs(30))?,
        "control: bob's one-chunk file never committed over the slow link"
    );

    // Trigger: bob rewrites big with the identical bytes; every chunk is already in the base, so
    // nothing uploads and the only work left is the one Commit carrying the ~256-chunk list.
    thread::sleep(crate::VISIBLE_AFTER);
    write_reps(&lab.mount_path("b").join("big"), &block, REPS)?;
    let landed = committed_within(&lab, "b", WATCH)?;
    let stats = lab.stats("b")?;
    assert!(
        landed,
        "bob's rewrite of a {} MiB, ~{REPS}-chunk file never committed after {WATCH:?} \
         (no bytes uploaded): {stats}",
        (BLOCK * REPS) >> 20
    );
    Ok(())
}

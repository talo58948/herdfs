//! Round 3: what an open file reads while its client switches it to a newer version and another
//! read of the same file is still in flight.

use std::fs::{self, File};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, read_at, save};

const LEN: usize = 16 << 20;
const PAGE: usize = 4096;
/// The long read: the first 12 MiB, in one read(2), which takes seconds on b's store link.
const LONG: usize = 12 << 20;
/// How far the long read gets before the stat: past its first chunk, which it has returned.
const STAT_AFTER: Duration = Duration::from_millis(1_500);
/// b's store link: about a second per 1 MiB chunk, so a cold read stays in flight a while.
const SLOW_STORE: Profile = Profile::Custom {
    round_trip_ms: 20,
    kilobytes_per_sec: 1_000,
    store_first_byte_ms: 0,
};

/// Both versions of `f`, and what b read in [`switch_during_a_long_read`].
struct Reads {
    old: Vec<u8>,
    new: Vec<u8>,
    long_read: Vec<u8>,
    after_stat: Vec<u8>,
}

impl Reads {
    /// Which version `bytes`, read at offset `at`, belong to.
    fn version(&self, bytes: &[u8], at: usize) -> &'static str {
        match bytes {
            bytes if bytes == &self.new[at..at + bytes.len()] => "v2",
            bytes if bytes == &self.old[at..at + bytes.len()] => "v1",
            _ => "neither",
        }
    }
}

/// On b, opens a 16 MiB `f` at v1 and reads its last page; a commits v2 of the same size; after
/// T_vis one thread on b reads the first 12 MiB through the handle in one read(2), about a second
/// per MiB, while another stats `f` 1.5 s into that read and then reads the last page again.
fn switch_during_a_long_read() -> Result<Option<Reads>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], SLOW_STORE, Strategy::Stream);
    let lab = Lab::start(config)?;
    let (old, new) = (random_bytes(LEN), random_bytes(LEN));
    let last = (LEN - PAGE) as u64;
    save(&lab.mount_path("a").join("f"), &old)?;
    thread::sleep(VISIBLE_AFTER);
    let path = lab.mount_path("b").join("f");
    let file = File::open(&path)?;
    let first = read_at(&file, last, PAGE)?;
    assert_bytes(&first, &old[LEN - PAGE..], "the last page, at v1");

    save(&lab.mount_path("a").join("f"), &new)?;
    thread::sleep(VISIBLE_AFTER);
    let (after_stat, long_read) = thread::scope(|scope| {
        let long_read = scope.spawn(|| read_long(&file));
        thread::sleep(STAT_AFTER);
        let after_stat = fs::metadata(&path).and_then(|_| read_at(&file, last, PAGE));
        (after_stat, long_read.join())
    });
    let long_read = long_read.map_err(|_| anyhow!("the long read panicked"))??;
    Ok(Some(Reads {
        old,
        new,
        long_read,
        after_stat: after_stat?,
    }))
}

/// The first [`LONG`] bytes of `file` through a single read(2), which must return them all.
fn read_long(file: &File) -> Result<Vec<u8>> {
    let mut bytes = vec![0; LONG];
    let read = std::os::unix::fs::FileExt::read_at(file, &mut bytes, 0)?;
    anyhow::ensure!(read == LONG, "one read(2) returned {read} of {LONG} bytes");
    Ok(bytes)
}

/// Repro: [`switch_during_a_long_read`]. The stat switches the inode to v2 and asks the kernel to
/// drop its pages (handles.rs `head`), but off the request and in page order, so the drop waits
/// behind the long read's pages in flight; the last page, still v1's, is served from the page
/// cache although the stat before it already switched to v2. Without the long read in flight the
/// same read returns v2 (round 2's `versions` tests).
// contract: C3 (docs/architecture.md: once a getattr sees a newer head, every handle on the inode
// reads the new version from its next read)
#[test]
#[ignore = "REDTEAM3: after a stat switched a file to a new version, its cached old pages are still read"]
fn a_read_after_a_stat_saw_a_new_version_never_returns_the_old_one() -> Result<()> {
    let Some(reads) = switch_during_a_long_read()? else {
        return Ok(());
    };
    assert_eq!(
        reads.version(&reads.after_stat, LEN - PAGE),
        "v2",
        "the last page, read after a stat that switched the inode to v2"
    );
    Ok(())
}

/// Repro: [`switch_during_a_long_read`]. The kernel serves the long read(2) as many FUSE reads,
/// each of which reads the version the inode follows when it arrives; the stat switches the inode
/// between two of them, so the one read(2) returns v1's first bytes and v2's after them.
// contract: C3 (a read never returns bytes of a partially committed version; an open file follows
// its latest version between reads)
#[test]
#[ignore = "REDTEAM3: one read(2) returns bytes spliced from two versions"]
fn one_read_returns_bytes_of_one_version() -> Result<()> {
    let Some(reads) = switch_during_a_long_read()? else {
        return Ok(());
    };
    let (bytes, old, new) = (&reads.long_read, &reads.old[..LONG], &reads.new[..LONG]);
    let verdict = match bytes.iter().zip(old).position(|(got, old)| got != old) {
        None => "v1".to_owned(),
        Some(_) if bytes == new => "v2".to_owned(),
        Some(at) if bytes[at..] == new[at..] => format!("v1 up to byte {at}, v2 from there"),
        Some(at) => format!("v1 up to byte {at}, then neither version"),
    };
    assert!(
        ["v1", "v2"].contains(&verdict.as_str()),
        "one read(2) of the first 12 MiB while the stat switched the inode: {verdict}"
    );
    Ok(())
}

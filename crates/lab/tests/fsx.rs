//! fsx, the file system exerciser, under each access strategy and under a chaos decider. fsx on
//! client A checks every read against its own model of the file. It keeps the file open
//! read-write, so it reads A's journal copy; client B, cold, then reads the committed file
//! through the strategy under test, and it must equal the same fsx run on a local directory.
//!
//! `FSX_OPS` sets the operations per run; `FSX_SEED` replays a seed. A run commits about 8
//! versions per 1,000 ops, most of the file each time, and the store keeps every chunk (C14):
//! the nightly 1,000,000 ops store about 33 GB per variant. So runs longer than the default take
//! turns, with other checkouts' too, and each skips unless the disk has room for its store.

use std::env;
use std::fs::{self, File};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

const DEFAULT_OPS: u64 = 50_000;
/// Store bytes per op, with a margin over the 33 KB measured at 50,000 ops.
const STORE_PER_OP: u64 = 40_000;
/// Each client's chunk cache: the file many times over. The default 1 GiB fills up with the
/// versions a reads back.
const CACHE_BYTES: u64 = 64 << 20;
const FILE: &str = "fsx.bin";
/// Read-only opens after the run; several, so a chaos decider picks more than one strategy.
const READS: u64 = 8;
/// Operations of fsx's log shown on a failure.
const LOG_TAIL: usize = 40;
/// T_vis plus a margin for the test's own scheduling.
const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);

#[test]
fn stream() -> Result<()> {
    exercise(|_| Strategy::Stream)
}

#[test]
fn fetch() -> Result<()> {
    exercise(|_| Strategy::Fetch)
}

#[test]
fn pin() -> Result<()> {
    exercise(|_| Strategy::Pin)
}

#[test]
fn index_large() -> Result<()> {
    exercise(|_| Strategy::Plan("index/large/no"))
}

#[test]
fn chaos() -> Result<()> {
    exercise(Strategy::Chaos)
}

/// Runs fsx on a mount whose strategy is chosen from the seed, then reads the result back.
fn exercise(strategy: impl FnOnce(u64) -> Strategy) -> Result<()> {
    // contract: C8, C9, P3, C1, C3, C2
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let seed = setting("FSX_SEED", rand::random())?;
    let ops = setting("FSX_OPS", DEFAULT_OPS)?;
    let long = ops > DEFAULT_OPS;
    let on_disk = long || env::var("LAB_MOUNT").as_deref() == Ok("nfs");
    let _turn = long.then(take_turn).transpose()?;
    if on_disk && !room_for_store(ops)? {
        return Ok(());
    }
    let strategy = strategy(seed);
    eprintln!("fsx seed {seed}, {ops} ops, {strategy:?}; replay with FSX_SEED={seed} LAB_KEEP=1");
    let clients = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, strategy)
        .client("b", "bob", &[], Profile::Lan, strategy)
        .cache_bytes("a", CACHE_BYTES)
        .cache_bytes("b", CACHE_BYTES);
    // A default run stores under 2 GB, which tmpfs holds, sparing the shared disk.
    let lab = Lab::start(if on_disk { clients } else { clients.on_tmpfs() })?;
    let local = tempfile::tempdir()?;
    fsx(local.path(), local.path(), seed, ops)?;
    let expected = fs::read(local.path().join(FILE))?;
    fsx(lab.mount_path("a"), lab.client_dir("a"), seed, ops)?;
    if env::var("LAB_MOUNT").as_deref() == Ok("nfs") {
        thread::sleep(VISIBLE_AFTER);
    }
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);

    let on_b = lab.mount_path("b").join(FILE);
    for read in 1..=READS {
        let bytes = fs::read(&on_b)?;
        ensure!(
            bytes == expected,
            "read {read} on b: {} bytes, the local run {}, first differing at {:?}",
            bytes.len(),
            expected.len(),
            bytes.iter().zip(&expected).position(|(got, want)| got != want)
        );
    }
    if std::env::var("LAB_MOUNT").as_deref() == Ok("nfs") {
        return Ok(());
    }
    let decisions = lab.stats("b")?["decisions"]["prefetch"].clone();
    let counts = decisions
        .as_object()
        .context("stats have no prefetch decisions")?
        .values();
    let decided: u64 = counts.filter_map(|count| count.as_u64()).sum();
    ensure!(decided == READS, "b decided {decisions} for {READS} opens");
    Ok(())
}

/// Runs fsx on `dir`/[`FILE`], keeping its artifacts in `artifacts`.
fn fsx(dir: &Path, artifacts: &Path, seed: u64, ops: u64) -> Result<()> {
    let config = if env::var("LAB_MOUNT").as_deref() == Ok("nfs") {
        "fsx-nfs.toml"
    } else {
        "fsx.toml"
    };
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join(config);
    let output = Command::new("fsx")
        .args(["-N", &ops.to_string(), "-S", &seed.to_string(), "-f"])
        .arg(config)
        .arg("-P")
        .arg(artifacts)
        .arg(dir.join(FILE))
        .output()
        .context("running fsx; install it with scripts/install-test-tools")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // fsx reports the mismatch, then dumps its recent operations.
    let (error, log) = stderr.split_once("LOG DUMP").unwrap_or((&stderr, ""));
    let log: Vec<&str> = log.lines().collect();
    let recent = log[log.len().saturating_sub(LOG_TAIL)..].join("\n");
    ensure!(
        output.status.success(),
        "fsx in {} failed ({}):\n{error}...\n{recent}",
        dir.display(),
        output.status
    );
    Ok(())
}

/// Waits until no other long fsx run on this host holds the turn, then holds it until dropped.
fn take_turn() -> Result<File> {
    let turn = File::create(env::temp_dir().join("herdfs-fsx-long-run.lock"))?;
    turn.lock()?;
    Ok(turn)
}

/// Whether the disk the lab lives on has room for the store of an `ops` run; says why not.
fn room_for_store(ops: u64) -> Result<bool> {
    let (dir, needed) = (env::temp_dir(), ops * STORE_PER_OP);
    let report = herdfs_lab::run(Command::new("df").args(["--output=avail", "-B1"]).arg(&dir))?;
    let free: u64 = report
        .lines()
        .last()
        .context("df printed nothing")?
        .trim()
        .parse()?;
    let gb = |bytes: u64| bytes / 1_000_000_000;
    if free < needed {
        let (needed, free) = (gb(needed), gb(free));
        eprintln!(
            "skipped: {ops} fsx ops need {needed} GB free for the store; {} has {free} GB",
            dir.display()
        );
    }
    Ok(free >= needed)
}

/// The number in environment variable `name`, or `default`.
fn setting(name: &str, default: u64) -> Result<u64> {
    env::var(name).map_or(Ok(default), |value| {
        value.parse().with_context(|| format!("{name}={value:?}"))
    })
}

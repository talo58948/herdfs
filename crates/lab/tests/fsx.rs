//! fsx, the file system exerciser, under each access strategy and under a chaos decider. fsx on
//! client A checks every read against its own model of the file. It keeps the file open
//! read-write, so it reads A's journal copy; client B, cold, then reads the committed file
//! through the strategy under test, and it must equal the same fsx run on a local directory.
//!
//! `FSX_OPS` sets the operations per run (nightly: 1,000,000); `FSX_SEED` replays a seed.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

const DEFAULT_OPS: u64 = 50_000;
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
fn chaos() -> Result<()> {
    exercise(Strategy::Chaos)
}

/// Runs fsx on a mount whose strategy is chosen from the seed, then reads the result back.
fn exercise(strategy: impl FnOnce(u64) -> Strategy) -> Result<()> {
    // contract: C8, C1, C3, C2
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let seed = setting("FSX_SEED", rand::random())?;
    let ops = setting("FSX_OPS", DEFAULT_OPS)?;
    let strategy = strategy(seed);
    eprintln!("fsx seed {seed}, {ops} ops, {strategy:?}; replay with FSX_SEED={seed}");
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, strategy)
            .client("b", "bob", &[], Profile::Lan, strategy),
    )?;
    let local = tempfile::tempdir()?;
    fsx(local.path(), local.path(), seed, ops)?;
    let expected = fs::read(local.path().join(FILE))?;
    fsx(lab.mount_path("a"), lab.client_dir("a"), seed, ops)?;
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
    let decisions = lab.stats("b")?["decisions"].clone();
    let counts = decisions.as_object().context("stats have no decisions")?.values();
    let decided: u64 = counts.filter_map(|count| count.as_u64()).sum();
    ensure!(decided == READS, "b decided {decisions} for {READS} opens");
    Ok(())
}

/// Runs fsx on `dir`/[`FILE`], keeping its artifacts in `artifacts`.
fn fsx(dir: &Path, artifacts: &Path, seed: u64, ops: u64) -> Result<()> {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("fsx.toml");
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

/// The number in environment variable `name`, or `default`.
fn setting(name: &str, default: u64) -> Result<u64> {
    env::var(name).map_or(Ok(default), |value| {
        value.parse().with_context(|| format!("{name}={value:?}"))
    })
}

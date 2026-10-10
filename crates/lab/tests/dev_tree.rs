//! Ring R3, U4: a developer tree. ripgrep, cloned with git at a pinned tag on `maker`, builds with
//! cargo on `builder`, its target directory inside the tree; afterwards git on `maker` finds the
//! clone clean and intact. Also times a walk with `stat` of every entry on cold clients over LAN
//! and WAN, a first look at many small files, and the same clone and build on local disk.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy, run};

const REPOSITORY: &str = "https://github.com/BurntSushi/ripgrep";
const TAG: &str = "14.1.1";
const TREE: &str = "ripgrep";
/// T_vis plus a margin for the test's own scheduling.
const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);

#[test]
fn a_cloned_tree_builds_on_another_client() -> Result<()> {
    // contract: C1, C2, C7
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("maker", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("builder", "bob", &[], Profile::Lan, Strategy::Stream)
            .client("walker", "carol", &[], Profile::Wan, Strategy::Stream),
    )?;
    let clone = lab.mount_path("maker").join(TREE);
    let cloned = clone_tag(&clone)?;
    lab.wait_committed("maker")?;
    thread::sleep(VISIBLE_AFTER);

    let lan = walk(&lab.mount_path("builder").join(TREE))?;
    let wan = walk(&lab.mount_path("walker").join(TREE))?;
    let built = build(&lab.mount_path("builder").join(TREE))?;
    // git reads back what it wrote through renames: its index, refs and packs (C1, C7).
    let git = |args: &[&str]| run(Command::new("git").args(args).current_dir(&clone));
    // macOS stores NFS resource-fork metadata in ordinary AppleDouble sidecars.
    assert_eq!(
        git(&["status", "--porcelain", "--", ".", ":(exclude,glob)**/._*"])?,
        ""
    );
    git(&["fsck"])?;

    let scratch = tempfile::tempdir()?;
    let local = scratch.path().join(TREE);
    let (cloned_locally, built_locally) = (clone_tag(&local)?, build(&local)?);
    eprintln!(
        "U4 developer tree: {TREE} {TAG}, {} entries; cloned in {cloned:.1?} (local disk \
         {cloned_locally:.1?}); built in {built:.1?} (local disk {built_locally:.1?}); cold walk \
         lan {:.1?} (stat p50/p99 {:.2?}/{:.2?}), wan {:.1?} ({:.2?}/{:.2?})",
        lan.entries, lan.took, lan.p50, lan.p99, wan.took, wan.p50, wan.p99
    );
    Ok(())
}

/// Clones the pinned tag into `dest` and returns how long that took.
fn clone_tag(dest: &Path) -> Result<Duration> {
    let started = Instant::now();
    run(Command::new("git")
        .args(["clone", "--quiet", "--branch", TAG, REPOSITORY])
        .arg(dest))?;
    Ok(started.elapsed())
}

/// Builds `tree` into `tree/target`, checks the binary runs, and returns how long the build took.
fn build(tree: &Path) -> Result<Duration> {
    let cargo = || {
        let mut cargo = Command::new("cargo");
        // rustc's incremental cache hard-links files, which herdfs refuses (out of v0), and
        // its fallback copies every object file one by one: over 14 minutes on WAN, unfinished.
        cargo
            .current_dir(tree)
            .env_remove("CARGO_TARGET_DIR")
            .env("CARGO_INCREMENTAL", "0");
        cargo
    };
    run(cargo().args(["fetch", "--locked"]))?;
    let started = Instant::now();
    run(cargo().args(["build", "--offline", "--locked", "--jobs", "4"]))?;
    let took = started.elapsed();
    let version = run(Command::new(tree.join("target/debug/rg")).arg("--version"))?;
    ensure!(version.starts_with(&format!("ripgrep {TAG} ")), "{version}");
    Ok(took)
}

/// A walk of a tree: its length, and the latencies of `lstat` on every entry.
struct Walk {
    entries: usize,
    took: Duration,
    p50: Duration,
    p99: Duration,
}

/// Walks the tree at `root` depth first, timing `lstat` of every entry.
fn walk(root: &Path) -> Result<Walk> {
    let started = Instant::now();
    let mut times = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let stat_started = Instant::now();
            let metadata = fs::symlink_metadata(&path)?;
            times.push(stat_started.elapsed());
            if metadata.is_dir() {
                dirs.push(path);
            }
        }
    }
    let took = started.elapsed();
    ensure!(!times.is_empty(), "{} is empty", root.display());
    times.sort();
    let percentile = |percent: usize| times[(times.len() - 1) * percent / 100];
    Ok(Walk {
        entries: times.len(),
        took,
        p50: percentile(50),
        p99: percentile(99),
    })
}

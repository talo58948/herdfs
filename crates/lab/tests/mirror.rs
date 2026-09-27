//! Cross-client mirror check: seeded random writes, truncates and renames on client A, the same
//! operations on a local directory, and then every path and byte seen on client B must match
//! that directory. `MIRROR_SEED` replays a seed.

use std::env;
use std::fs::{self, File};
use std::io;
use std::iter;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};

const OPS: usize = 200;
/// File names per directory; few, so writes often hit existing files and renames replace them.
const NAMES: usize = 4;
/// Files grow past a few chunks (1 MiB on average), with holes.
const MAX_OFFSET: u64 = 3 << 20;
const MAX_WRITE: usize = 256 << 10;
/// T_vis plus a margin for the test's own scheduling.
const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);

#[derive(Debug)]
enum Op {
    /// Writes `len` bytes, drawn from a generator seeded with `fill`, creating the file if needed.
    Write {
        path: PathBuf,
        offset: u64,
        len: usize,
        fill: u64,
    },
    Truncate {
        path: PathBuf,
        size: u64,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
}

#[test]
fn client_b_sees_what_a_local_directory_holds_after_the_same_operations() -> Result<()> {
    // contract: C2, C7, C8
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let seed = match env::var("MIRROR_SEED") {
        Ok(seed) => seed.parse().with_context(|| format!("MIRROR_SEED={seed:?}"))?,
        Err(_) => rand::random(),
    };
    eprintln!("mirror seed {seed}; replay with MIRROR_SEED={seed}");
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Chaos(seed)),
    )?;
    let mirror = tempfile::tempdir()?;
    let roots = [lab.mount_path("a"), mirror.path()];
    for root in roots {
        fs::create_dir_all(root.join("d0/d1"))?;
    }
    let mut rng = SmallRng::seed_from_u64(seed);
    for step in 0..OPS {
        let op = random_op(&mut rng, mirror.path(), step)?;
        eprintln!("{step}: {op:?}");
        for root in roots {
            op.apply(root)
                .with_context(|| format!("{op:?} in {}", root.display()))?;
        }
    }
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);

    let on_b = lab.mount_path("b");
    let (listed, paths) = (listing(on_b)?, listing(mirror.path())?);
    ensure!(listed == paths, "b lists {listed:?}\nthe mirror holds {paths:?}");
    for (path, _) in paths.iter().filter(|(_, is_dir)| !is_dir) {
        let (got, want) = (fs::read(on_b.join(path))?, fs::read(mirror.path().join(path))?);
        ensure!(
            got == want,
            "{}: b reads {} bytes, the mirror has {}, first differing at {:?}",
            path.display(),
            got.len(),
            want.len(),
            got.iter().zip(&want).position(|(got, want)| got != want)
        );
    }
    Ok(())
}

/// An operation on what the mirror holds now: mostly writes, which may create a file.
fn random_op(rng: &mut SmallRng, mirror: &Path, step: usize) -> Result<Op> {
    let paths = listing(mirror)?;
    let subdirs = paths.iter().filter(|(_, is_dir)| *is_dir).map(|(path, _)| path);
    let dirs: Vec<&Path> = iter::once(Path::new(""))
        .chain(subdirs.map(PathBuf::as_path))
        .collect();
    let files: Vec<&Path> = paths
        .iter()
        .filter(|(_, is_dir)| !is_dir)
        .map(|(path, _)| path.as_path())
        .collect();
    let name = |rng: &mut SmallRng| pick(rng, &dirs).join(format!("f{}", rng.random_range(0..NAMES)));
    let op = match rng.random_range(0..10) {
        kind if kind < 5 || files.is_empty() => Op::Write {
            path: name(rng),
            offset: rng.random_range(0..MAX_OFFSET),
            len: rng.random_range(1..=MAX_WRITE),
            fill: rng.random(),
        },
        5..7 => Op::Truncate {
            path: pick(rng, &files).to_path_buf(),
            size: rng.random_range(0..MAX_OFFSET),
        },
        7..9 => Op::Rename {
            from: pick(rng, &files).to_path_buf(),
            to: name(rng),
        },
        _ => {
            // Moves a directory, and so its subtree, under one that is not inside it (C7).
            let from = *pick(rng, &dirs[1..]);
            let outside: Vec<&Path> = dirs
                .iter()
                .copied()
                .filter(|dir| !dir.starts_with(from))
                .collect();
            let to = pick(rng, &outside).join(format!("d{step}"));
            let from = from.to_path_buf();
            Op::Rename { from, to }
        }
    };
    Ok(op)
}

fn pick<'a, T>(rng: &mut SmallRng, items: &'a [T]) -> &'a T {
    &items[rng.random_range(0..items.len())]
}

impl Op {
    fn apply(&self, root: &Path) -> io::Result<()> {
        match self {
            Self::Write {
                path,
                offset,
                len,
                fill,
            } => {
                let mut bytes = vec![0; *len];
                SmallRng::seed_from_u64(*fill).fill(&mut bytes[..]);
                let file = File::options()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(root.join(path))?;
                file.write_all_at(&bytes, *offset)
            }
            Self::Truncate { path, size } => {
                File::options().write(true).open(root.join(path))?.set_len(*size)
            }
            Self::Rename { from, to } => fs::rename(root.join(from), root.join(to)),
        }
    }
}

/// Every path below `root`, relative to it and sorted, with whether it is a directory.
fn listing(root: &Path) -> Result<Vec<(PathBuf, bool)>> {
    let mut found = Vec::new();
    let mut unvisited = vec![PathBuf::new()];
    while let Some(dir) = unvisited.pop() {
        for entry in fs::read_dir(root.join(&dir))? {
            let entry = entry?;
            let (path, is_dir) = (dir.join(entry.file_name()), entry.file_type()?.is_dir());
            if is_dir {
                unvisited.push(path.clone());
            }
            found.push((path, is_dir));
        }
    }
    found.sort();
    Ok(found)
}

//! U8: forking a space at 10k and at 50k entries, then writing in a fork through its own mount.

use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::time::{Duration, Instant};
use std::{panic, thread};

use anyhow::{Result, bail};
use herdfs_core::proto::{ErrorCode, Request, Response};
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, SPACE, Strategy};

use crate::support::{self, VISIBLE_AFTER, counter, errno, noise, path, save};

const SMALL: usize = 10_000;
const LARGE: usize = 50_000;
/// Forks timed at each size; the fastest counts, as the least disturbed by other work.
const TRIES: usize = 3;
const DIRS: usize = 100;
/// Concurrent callers while filling the space.
const FILLERS: usize = 8;
/// C13: at most 1 s per 100k entries.
const BOUND_PER_ENTRY: Duration = Duration::from_micros(10);
/// How much the time per entry may grow from the small space to the large one. It grows 1.4 to
/// 1.8 times as the database outgrows caches; quadratic time would show as 5.
const LINEAR_SLACK: f64 = 2.5;
const BIG: &str = "big.bin";
const BIG_LEN: usize = 32 << 20;
const WRITES: usize = 100;
/// Logs, the token and filesystem block rounding on top of the cache.
const DISK_SLACK: u64 = 1 << 20;

#[test]
fn a_fork_takes_linear_time_and_leaves_its_parent_alone() -> Result<()> {
    // contract: C10, C13
    let Some(mut lab) = support::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
            .on_tmpfs(),
    )?
    else {
        return Ok(());
    };
    let big = noise(BIG_LEN);
    save(&lab.mount_path("a").join(BIG), &big)?;
    for dir in 0..DIRS {
        let (space, path, mode) = (SPACE.into(), path(&format!("d{dir:02}"))?, 0o755);
        lab.rpc(ADMIN, &Request::Mkdir { space, path, mode })?;
    }
    fill(&lab, DIRS..SMALL)?;
    let small = fork_per_entry(&lab, SMALL)?;
    fill(&lab, SMALL..LARGE)?;
    let large = fork_per_entry(&lab, LARGE)?;
    assert!(large <= BOUND_PER_ENTRY.as_secs_f64(), "{large} s per entry");
    assert!(large <= small * LINEAR_SLACK, "{small} then {large} s per entry");

    let before = parent_state(&lab)?;
    lab.mount_space("b", &fork_name(LARGE, 0))?;
    let on_b = lab.mount_path("b");
    let mut piece = vec![0; 1 << 20];
    File::open(on_b.join(BIG))?.read_exact_at(&mut piece, BIG_LEN as u64 / 2)?;
    assert_eq!(piece, big[BIG_LEN / 2..][..piece.len()]);
    fs::create_dir(on_b.join("new"))?;
    for n in 0..WRITES {
        let text = format!("fork file {n}\n");
        save(&on_b.join(format!("new/f{n}")), text.as_bytes())?;
    }
    save(&on_b.join(BIG), b"rewritten in the fork\n")?;
    assert_eq!(parent_state(&lab)?, before);

    save(&lab.mount_path("a").join("after-fork.txt"), b"parent only\n")?;
    thread::sleep(VISIBLE_AFTER);
    let after_fork = fs::metadata(on_b.join("after-fork.txt"));
    assert_eq!(errno(after_fork), Some(libc::ENOENT));

    assert_eq!(counter(&lab, "b", "pending_commits")?, 0);
    let fetched = counter(&lab, "b", "bytes_fetched")?;
    let used = disk_use(lab.client_dir("b"))?;
    eprintln!("u8: the fork's mount uses {used} bytes of disk after fetching {fetched}");
    assert!(used <= fetched + DISK_SLACK, "{used} bytes on disk");
    Ok(())
}

/// Adds files numbered `files` to the lab's space, spread over its directories, each with one
/// empty version.
fn fill(lab: &Lab, files: Range<usize>) -> Result<()> {
    let files: Vec<usize> = files.collect();
    // One token for all, as a client holds one, saves signing a token per call.
    let token = &lab.token(ADMIN, &[]);
    thread::scope(|scope| {
        let fillers: Vec<_> = files
            .chunks(files.len().div_ceil(FILLERS))
            .map(|share| scope.spawn(move || share.iter().try_for_each(|&n| add_file(lab, token, n))))
            .collect();
        fillers
            .into_iter()
            .try_for_each(|filler| filler.join().unwrap_or_else(|panic| panic::resume_unwind(panic)))
    })
}

fn add_file(lab: &Lab, token: &str, n: usize) -> Result<()> {
    let create = Request::Create {
        space: SPACE.into(),
        path: path(&format!("d{:02}/f{n}", n % DIRS))?,
        mode: 0o644,
    };
    let Response::Entry { entry } = lab.rpc_with(token, &create)? else {
        bail!("Create answered something else");
    };
    let commit = Request::Commit {
        space: SPACE.into(),
        entry: entry.id,
        request_id: format!("fill-{n}"),
        base: None,
        guarded: false,
        chunks: Vec::new(),
    };
    lab.rpc_with(token, &commit)?;
    Ok(())
}

fn fork_name(entries: usize, attempt: usize) -> String {
    format!("fork-{entries}-{attempt}")
}

/// Forks the lab's space, which has about `entries` entries, `TRIES` times and returns the
/// fastest fork's seconds per entry.
fn fork_per_entry(lab: &Lab, entries: usize) -> Result<f64> {
    let mut fastest = Duration::MAX;
    for attempt in 0..TRIES {
        let (space, name) = (SPACE.into(), fork_name(entries, attempt));
        let start = Instant::now();
        lab.rpc(ADMIN, &Request::Fork { space, name })?;
        let took = start.elapsed();
        eprintln!("u8: fork of {entries} entries took {took:?}");
        fastest = fastest.min(took);
    }
    Ok(fastest.as_secs_f64() / entries as f64)
}

/// What writes in the fork must not change in its parent: its root listing, the big file's
/// history, and the absence of the fork's `new` directory.
fn parent_state(lab: &Lab) -> Result<[Result<Response, ErrorCode>; 3]> {
    let space = || SPACE.to_owned();
    let requests = [
        Request::List {
            space: space(),
            path: path("")?,
        },
        Request::History {
            space: space(),
            path: path(BIG)?,
        },
        Request::Stat {
            space: space(),
            path: path("new")?,
        },
    ];
    Ok(requests.map(|request| lab.rpc(ADMIN, &request).map_err(|error| error.code)))
}

/// Bytes of disk allocated to the files under `dir`.
fn disk_use(dir: &Path) -> io::Result<u64> {
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

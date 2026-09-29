//! U8: forking a space at 10k and at 50k entries, then writing in a fork through its own mount.

use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::time::{Duration, Instant};
use std::{panic, thread};

use anyhow::{Context, Result, bail};
use herdfs_core::proto::{EntryId, ErrorCode, Request, Response};
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, SPACE, Strategy};

use crate::support::{self, VISIBLE_AFTER, counter, errno, noise, path, save};

const SMALL: usize = 10_000;
const LARGE: usize = 50_000;
/// Forks timed at each size, so the 10 ms clock ticks of CPU time are a small error.
const TRIES: usize = 3;
/// Clock ticks per second in `/proc/<pid>/stat` (USER_HZ), fixed at 100 on Linux.
const CLOCK_TICKS: u64 = 100;
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
    let root = entry_at(&lab, "")?;
    let mut dirs = Vec::with_capacity(DIRS);
    for dir in 0..DIRS {
        let (space, path, mode) = (SPACE.into(), path(&format!("d{dir:02}"))?, 0o755);
        let Response::Entry { entry } = lab.rpc(
            ADMIN,
            &Request::Mkdir {
                space,
                parent: root,
                name: path.name().into(),
                mode,
            },
        )?
        else {
            bail!("Mkdir answered something else")
        };
        dirs.push(entry.id);
    }
    fill(&lab, &dirs, DIRS..SMALL)?;
    let small = fork_per_entry(&lab, SMALL)?;
    fill(&lab, &dirs, SMALL..LARGE)?;
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
fn fill(lab: &Lab, dirs: &[EntryId], files: Range<usize>) -> Result<()> {
    let files: Vec<usize> = files.collect();
    // One token for all, as a client holds one, saves signing a token per call.
    let token = &lab.token(ADMIN, &[]);
    thread::scope(|scope| {
        let fillers: Vec<_> = files
            .chunks(files.len().div_ceil(FILLERS))
            .map(|share| scope.spawn(move || share.iter().try_for_each(|&n| add_file(lab, token, dirs, n))))
            .collect();
        fillers
            .into_iter()
            .try_for_each(|filler| filler.join().unwrap_or_else(|panic| panic::resume_unwind(panic)))
    })
}

fn add_file(lab: &Lab, token: &str, dirs: &[EntryId], n: usize) -> Result<()> {
    let create = Request::Create {
        space: SPACE.into(),
        parent: dirs[n % DIRS],
        name: format!("f{n}"),
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
        mtime_ms: None,
    };
    lab.rpc_with(token, &commit)?;
    Ok(())
}

fn entry_at(lab: &Lab, at: &str) -> Result<EntryId> {
    let stat = Request::Stat {
        space: SPACE.into(),
        path: path(at)?,
    };
    match lab.rpc(ADMIN, &stat)? {
        Response::Entry { entry } => Ok(entry.id),
        other => bail!("Stat answered {other:?}"),
    }
}

fn fork_name(entries: usize, attempt: usize) -> String {
    format!("fork-{entries}-{attempt}")
}

/// Forks the lab's space, which has about `entries` entries, `TRIES` times and returns the
/// authority's CPU seconds per entry forked. CPU time rather than wall time, because on a shared
/// box the wall time of a fork also counts every other process's turns on the CPU.
fn fork_per_entry(lab: &Lab, entries: usize) -> Result<f64> {
    let pid = lab.authority_pid().context("the authority runs")?;
    let before = cpu_time(pid)?;
    for attempt in 0..TRIES {
        let (space, name) = (SPACE.into(), fork_name(entries, attempt));
        let start = Instant::now();
        lab.rpc(ADMIN, &Request::Fork { space, name })?;
        eprintln!("u8: fork of {entries} entries took {:?}", start.elapsed());
    }
    let spent = cpu_time(pid)? - before;
    eprintln!("u8: {TRIES} forks of {entries} entries took {spent:?} of CPU");
    Ok(spent.as_secs_f64() / (TRIES * entries) as f64)
}

/// User and system CPU time of process `pid` and all its threads, living or gone.
fn cpu_time(pid: u32) -> Result<Duration> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // After the command name come the state, then fields 4 to 13, then utime and stime.
    let (_, fields) = stat.rsplit_once(") ").context("malformed /proc stat")?;
    let fields: Vec<&str> = fields.split(' ').collect();
    let ticks: u64 = fields[11].parse::<u64>()? + fields[12].parse::<u64>()?;
    Ok(Duration::from_millis(ticks * 1000 / CLOCK_TICKS))
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

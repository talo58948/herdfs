//! Ring R3, U1 and U2: a ~1 GiB MP4 made with ffmpeg on `maker`, then probed with `ffprobe` on
//! `probe` and read at random on `seek`, both over a WAN link with a cold cache. The scenarios
//! share one test because making and committing the file takes over a minute.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::ops::Range;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use herdfs_core::proto::Version;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy, overlapping_chunk_bytes, run};
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};

const MOVIE: &str = "movie.mp4";
/// 66 s of lossless 4K test pattern, about 1 GiB at 130 Mbit/s and quick to encode. The tee
/// muxer writes one encode to several files.
const ENCODE: &str = "-v error -f lavfi -i testsrc2=size=3840x2160:rate=30 -t 66 \
    -c:v libx264 -preset ultrafast -qp 0 -threads 4 -map 0:v -f tee";
const PROBE: &str = "ffprobe -v error -show_entries stream=codec_name:format=size \
    -of default=noprint_wrappers=1";
const READS: usize = 50;
const READ_LEN: u64 = 1 << 20;
const SEED: u64 = 7;
const PAGE: u64 = 4096;
/// T_vis plus a margin for the test's own scheduling.
const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);

#[test]
fn media_reads_fetch_only_the_chunks_they_touch() -> Result<()> {
    // contract: C9, C11
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("maker", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("probe", "bob", &[], Profile::Wan, Strategy::Stream)
            .client("seek", "carol", &[], Profile::Wan, Strategy::Stream),
    )?;
    let scratch = tempfile::tempdir()?;
    let mirror = scratch.path().join(MOVIE);
    make_movie(&lab, &mirror)?;
    let movie = lab.head("alice", MOVIE)?;
    assert_eq!(movie.size, fs::metadata(&mirror)?.len());
    thread::sleep(VISIBLE_AFTER);
    probe_header(&lab, &movie, scratch.path())?;
    read_at_random(&lab, &movie, &mirror)?;
    Ok(())
}

/// Encodes the movie once and muxes it into both `mirror` and `maker`'s mount, then waits for
/// the commit.
fn make_movie(lab: &Lab, mirror: &Path) -> Result<()> {
    let movie = lab.mount_path("maker").join(MOVIE);
    let outputs = [mirror, &movie]
        .map(|path| format!("[f=mp4:movflags=+faststart]{}", path.display()))
        .join("|");
    let started = Instant::now();
    run(Command::new("ffmpeg").args(ENCODE.split(' ')).arg(outputs))?;
    let encoded = started.elapsed();
    // fsync returns once the authority has committed the version.
    File::open(&movie)?.sync_all()?;
    let committed = started.elapsed() - encoded;
    let size = fs::metadata(mirror)?.len();
    eprintln!("U1 and U2 setup: ffmpeg made {size} B in {encoded:.1?}; commit took {committed:.1?}");
    Ok(())
}

/// U1: `ffprobe` fetches only the chunks under the bytes it reads, plus the kernel's readahead.
fn probe_header(lab: &Lab, version: &Version, scratch: &Path) -> Result<()> {
    let (size, chunks) = (version.size, &version.chunks);
    let movie = lab.mount_path("probe").join(MOVIE);
    let trace = scratch.join("ffprobe.strace");
    let started = Instant::now();
    let report = run(Command::new("strace")
        .args(["-y", "-s", "0", "-o"])
        .arg(&trace)
        .args(["-e", "trace=openat,lseek,read,pread64,readv,preadv,preadv2,mmap"])
        .args(PROBE.split(' '))
        .arg(&movie))?;
    let elapsed = started.elapsed();
    assert_eq!(report, format!("codec_name=h264\nsize={size}\n"));

    let reads = ranges_read(&fs::read_to_string(&trace)?, &movie)?;
    // Buffered reads go through the kernel's page cache, which reads whole pages and, as the
    // mount's configured prefetch, up to two readahead windows past the last page read.
    let readahead = 2 * readahead_bytes(lab.mount_path("probe"))?;
    let widened: Vec<_> = (reads.iter())
        .map(|read| read.start / PAGE * PAGE..read.end.next_multiple_of(PAGE) + readahead)
        .collect();
    let least = overlapping_chunk_bytes(chunks, &reads);
    let most = overlapping_chunk_bytes(chunks, &widened);
    let fetched = bytes_fetched(lab, "probe")?;
    assert!(
        (least..=most).contains(&fetched),
        "fetched {fetched} B; the reads {reads:?} need {least} B and may take {most} B"
    );
    assert!(fetched < size / 100, "fetched {fetched} of {size} B");
    let read: u64 = reads.iter().map(|read| read.end - read.start).sum();
    eprintln!(
        "U1 ffprobe: {size} B in {} chunks; read {read} B in {} reads; fetched {fetched} B \
         (bound {least}..={most}) in {elapsed:.2?}",
        chunks.len(),
        reads.len()
    );
    Ok(())
}

/// U2: seeded random 1 MiB reads fetch exactly the distinct chunks they overlap and return the
/// mirror's bytes, cold and again from the cache.
fn read_at_random(lab: &Lab, version: &Version, mirror: &Path) -> Result<()> {
    let (size, chunks) = (version.size, &version.chunks);
    let mut rng = SmallRng::seed_from_u64(SEED);
    let reads: Vec<Range<u64>> = (0..READS)
        .map(|_| rng.random_range(0..=size - READ_LEN))
        .map(|start| start..start + READ_LEN)
        .collect();
    // O_DIRECT bypasses the page cache, so the kernel neither reads ahead nor serves repeats.
    let movie = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(lab.mount_path("seek").join(MOVIE))?;
    let mirror = File::open(mirror)?;

    let cold = read_and_compare(&movie, &mirror, &reads)?;
    let fetched = bytes_fetched(lab, "seek")?;
    let touched = overlapping_chunk_bytes(chunks, &reads);
    assert_eq!(fetched, touched, "seed {SEED}");
    let warm = read_and_compare(&movie, &mirror, &reads)?;
    assert_eq!(bytes_fetched(lab, "seek")?, fetched, "a warm read fetched");
    let count = lab.stats("seek")?["chunks_fetched"].clone();
    eprintln!(
        "U2 random reads: {READS} x {READ_LEN} B (seed {SEED}) over {size} B; fetched {fetched} B \
         in {count} chunks = the chunks touched; cold {cold:.2?}, warm {warm:.2?}"
    );
    Ok(())
}

/// Reads each range from `file`, checks it against `mirror`, and returns the time spent reading.
fn read_and_compare(file: &File, mirror: &File, reads: &[Range<u64>]) -> Result<Duration> {
    let mut spent = Duration::ZERO;
    for read in reads {
        let mut got = vec![0; usize::try_from(read.end - read.start)?];
        let mut want = got.clone();
        let started = Instant::now();
        file.read_exact_at(&mut got, read.start)?;
        spent += started.elapsed();
        mirror.read_exact_at(&mut want, read.start)?;
        assert!(got == want, "bytes {read:?} differ from the mirror");
    }
    Ok(spent)
}

/// The byte ranges of `path` that a process read, from its `strace -y` log.
fn ranges_read(trace: &str, path: &Path) -> Result<Vec<Range<u64>>> {
    let tag = format!("<{}>", path.display());
    let mut positions: HashMap<u64, u64> = HashMap::new();
    let mut reads = Vec::new();
    for line in trace.lines().filter(|line| line.contains(&tag)) {
        let call = Call::parse(line).with_context(|| format!("unexpected strace line {line:?}"))?;
        let position = positions.entry(call.fd).or_default();
        match (call.name, call.last_arg) {
            ("openat", _) => *position = 0,
            ("lseek", _) => *position = call.result,
            ("read", _) => {
                reads.push(*position..*position + call.result);
                *position += call.result;
            }
            ("pread64", Some(offset)) => reads.push(offset..offset + call.result),
            _ => bail!("unexpected strace line {line:?}"),
        }
    }
    ensure!(!reads.is_empty(), "no reads of {} in the trace", path.display());
    Ok(reads)
}

/// A successful system call on a file descriptor, as `strace -y` logs it.
struct Call<'a> {
    name: &'a str,
    fd: u64,
    last_arg: Option<u64>,
    result: u64,
}

impl<'a> Call<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let (name, rest) = line.split_once('(')?;
        let (args, result) = rest.rsplit_once(") = ")?;
        // A descriptor is logged as `4</path>`, so its number is what comes before `<`.
        let number = |text: &str| text.split('<').next()?.trim().parse().ok();
        let result = number(result)?;
        let fd = if name == "openat" { result } else { number(args)? };
        let last_arg = args.rsplit(", ").next().and_then(number);
        Some(Self {
            name,
            fd,
            last_arg,
            result,
        })
    }
}

/// The kernel's readahead window on `mount`, from its device's backing-device settings.
fn readahead_bytes(mount: &Path) -> Result<u64> {
    let device = run(Command::new("mountpoint").arg("-d").arg(mount))?;
    let setting = format!("/sys/class/bdi/{}/read_ahead_kb", device.trim());
    let kib: u64 = fs::read_to_string(&setting)?.trim().parse()?;
    Ok(kib * 1024)
}

fn bytes_fetched(lab: &Lab, name: &str) -> Result<u64> {
    lab.stats(name)?["bytes_fetched"]
        .as_u64()
        .context("stats lack bytes_fetched")
}

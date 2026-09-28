//! Round 3: when a closed write commits while its file keeps being closed.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, save};

/// How often the loop appends, well inside the committer's 250 ms quiet period.
const EVERY: Duration = Duration::from_millis(20);
/// How long after the first append b looks: the quiet period, a commit and T_vis, with room.
const LOOK_AFTER: Duration = Duration::from_secs(4);

/// Clients `a` and `b` as in `two_clients`, with their journals on tmpfs, so that the fsyncs of
/// each close take no time; `None` until the binaries are built.
fn two_clients_on_tmpfs() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .on_tmpfs()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream);
    Lab::start(config).map(Some)
}

/// Appends `line` to `path` through its own open and close, as `echo line >> path` does.
fn append(path: &Path, line: &str) -> io::Result<()> {
    OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

/// Repro: a appends a line to `log` every 20 ms, each through its own open and close, as a
/// shell loop of `echo line >> log` does; 4 s after the first line b reads `log`. Every seal
/// restarts the committer's 250 ms quiet period (journal.rs `QUIET`, commit.rs `next`), so
/// nothing commits while the appends go on, and b reads none of the lines a closed seconds ago.
// contract: none (docs/architecture.md: on `close` a background committer commits the copy; a
// file becomes visible to other hosts on close)
#[test]
fn lines_appended_in_a_loop_reach_other_clients() -> Result<()> {
    let Some(lab) = two_clients_on_tmpfs()? else {
        return Ok(());
    };
    let (log_a, log_b) = (lab.mount_path("a").join("log"), lab.mount_path("b").join("log"));
    save(&log_a, b"start\n")?;
    let stop = AtomicBool::new(false);
    let (seen, appended) = thread::scope(|scope| {
        let appender = scope.spawn(|| -> io::Result<(usize, Duration)> {
            let (mut lines, mut longest, mut last) = (0, Duration::ZERO, Instant::now());
            while !stop.load(Relaxed) {
                append(&log_a, &format!("line {lines}\n"))?;
                (lines, longest, last) = (lines + 1, longest.max(last.elapsed()), Instant::now());
                thread::sleep(EVERY);
            }
            Ok((lines, longest))
        });
        thread::sleep(LOOK_AFTER);
        let seen = fs::read_to_string(&log_b);
        stop.store(true, Relaxed);
        (seen, appender.join())
    });
    let (appended, longest) = appended.map_err(|_| anyhow::anyhow!("the appender panicked"))??;
    let seen = seen?;
    // Once the appends stop, the last seal commits and b sees every line.
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    let after = fs::read_to_string(&log_b)?.lines().count() - 1;
    assert!(
        seen.contains("line 0\n"),
        "b read {seen:?} {LOOK_AFTER:?} after a closed the first of {appended} lines, at most \
         {longest:?} apart; b read {after} lines once the appends stopped"
    );
    Ok(())
}

//! Round 2: the agents runner on a real mount, seen from the host that asked for a session.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, save};

const OUTPUT_LEN: usize = 20_000_000;
/// The session's own run time is small; this bounds the wait for its status.
const SESSION_DONE: Duration = Duration::from_secs(60);
const CANARY: &str = "canary-3f9c1e: only on host c's own disk";

/// A lab where client `a` asks host `c`, on `c_link`, for sessions, and session `t1`'s directory
/// as `a` sees it; `None` until the binaries are built.
fn requester_and_host(c_link: Profile) -> Result<Option<(Lab, PathBuf)>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("c", "carol", &[], c_link, Strategy::Stream);
    let lab = Lab::start(config)?;
    let session = lab.mount_path("a").join(".agents/c/t1");
    fs::create_dir_all(&session)?;
    Ok(Some((lab, session)))
}

/// Polls the session's `status` until the runner ends it, and returns it.
fn ended(session: &Path) -> Result<String> {
    let started = Instant::now();
    loop {
        match fs::read_to_string(session.join("status")) {
            // Empty until the runner's first write to it commits.
            Ok(status) if !matches!(status.trim_end(), "" | "running") => return Ok(status),
            Err(error) if error.kind() != ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        ensure!(
            started.elapsed() < SESSION_DONE,
            "not ended after {SESSION_DONE:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Repro: a asks host c, on a WAN link, for a session whose command prints 20 MB; a waits until
/// `status` says `done`, then T_vis more, and reads `output`. The runner writes `status` as soon
/// as the command exits, without waiting for `output` to commit, and the few bytes of `status`
/// commit long before the 20 MB of `output` have uploaded: the session is done and its output is
/// missing.
// contract: none (docs/architecture.md: other hosts see `output` when the session ends)
#[test]
#[ignore = "REDTEAM2: a session reads done on other hosts before its output is committed"]
fn a_done_session_shows_its_whole_output_on_other_hosts() -> Result<()> {
    let Some((mut lab, session)) = requester_and_host(Profile::Wan)? else {
        return Ok(());
    };
    lab.start_agents("c", &format!("head -c {OUTPUT_LEN} /dev/urandom"))?;
    save(&session.join("prompt"), b"print\n")?;
    let status = ended(&session)?;
    if status.trim_end() != "done" {
        bail!("the session ended {status:?}");
    }
    thread::sleep(VISIBLE_AFTER);
    let seen = fs::metadata(session.join("output"))?.len();
    lab.wait_committed("c")?;
    thread::sleep(VISIBLE_AFTER);
    let committed = fs::metadata(session.join("output"))?.len();
    assert_eq!(committed, OUTPUT_LEN as u64, "output once c has committed it");
    assert_eq!(seen, committed, "output T_vis after status said done");
    Ok(())
}

/// Repro: host c keeps a private file on its own disk; a makes session t1's `prompt` a symlink
/// to that file's absolute path; the runner's command is `cat`. The runner opens `prompt` through
/// the symlink, so c's private file becomes the session's output, which a reads.
// contract: A1 (a session is its directory in the mount; the runner already refuses a cwd outside it)
#[test]
#[ignore = "REDTEAM2: the agents runner follows session symlinks to the host's own files"]
fn a_session_never_reads_the_hosts_files_outside_the_mount() -> Result<()> {
    let Some((mut lab, session)) = requester_and_host(Profile::Lan)? else {
        return Ok(());
    };
    let private = lab.client_dir("c").join("private-note");
    fs::write(&private, CANARY)?;
    symlink(&private, session.join("prompt"))?;
    lab.start_agents("c", "cat")?;
    let status = ended(&session)?;
    lab.wait_committed("c")?;
    thread::sleep(VISIBLE_AFTER);
    let output = fs::read_to_string(session.join("output")).unwrap_or_default();
    assert!(
        !output.contains(CANARY),
        "the session ended {status:?} and a reads c's private file in its output"
    );
    Ok(())
}

/// Repro: a creates session t1's `prompt`, writes it, and keeps it open for 4 s before closing
/// it; the runner polls every second. The new, empty `prompt` is visible to c at once, its bytes
/// only once a's close commits them, and the runner takes a prompt unchanged across two polls as
/// finished: it runs the session on an empty prompt before a has closed it.
// contract: A1 (docs/architecture.md: the session holds `prompt`; a file becomes visible to other hosts on close)
#[test]
#[ignore = "REDTEAM2: the agents runner starts a session on a prompt still being written"]
fn a_session_runs_on_the_prompt_its_requester_wrote() -> Result<()> {
    let Some((mut lab, session)) = requester_and_host(Profile::Lan)? else {
        return Ok(());
    };
    lab.start_agents("c", "cat; echo ok")?;
    let mut prompt = fs::File::create(session.join("prompt"))?;
    prompt.write_all(b"hello\n")?;
    thread::sleep(Duration::from_secs(4));
    prompt.sync_all()?;
    drop(prompt);
    let status = ended(&session)?;
    lab.wait_committed("c")?;
    thread::sleep(VISIBLE_AFTER);
    let output = fs::read_to_string(session.join("output"))?;
    assert_eq!(
        output, "hello\nok\n",
        "the output of a session that ended {status:?}"
    );
    Ok(())
}

//! Round 30: the agents host-directory permission boundary through held directories.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, SPACE, Strategy};

const HOST: &str = ".agents/c";
const TRIALS: usize = 3;

fn set_acl(lab: &Lab, path: &str, perm: Option<Perm>) -> Result<()> {
    lab.rpc(
        ADMIN,
        &Request::SetAcl {
            space: SPACE.into(),
            path: path.try_into()?,
            principal: "user:bob".into(),
            perm,
        },
    )?;
    Ok(())
}

fn wait_done(dir: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match fs::read_to_string(dir.join("status")) {
            Ok(status) if status.trim() == "done" => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            outcome => ensure!(
                !matches!(outcome, Ok(ref status) if status.starts_with("failed:")),
                "session failed: {outcome:?}"
            ),
        }
        ensure!(Instant::now() < deadline, "session did not finish");
        thread::sleep(Duration::from_millis(50));
    }
}

/// A caller that retained session directory handles, but no longer has write on the host
/// directory, must not be able to submit prompts that the runner executes.
// contract: A1
#[test]
#[ignore = "ROUND30: held session directories bypass the agents host-directory write check"]
fn spawning_requires_current_write_access_on_the_host_directory() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
        .client("c", "carol", &[], Profile::Lan, Strategy::Stream);
    let mut lab = Lab::start(config)?;
    let (alice, bob) = (lab.mount_path("a").to_owned(), lab.mount_path("b").to_owned());
    fs::create_dir(alice.join(".agents"))?;
    fs::create_dir(alice.join(HOST))?;
    for trial in 0..TRIALS {
        fs::create_dir(alice.join(HOST).join(format!("attack-{trial}")))?;
    }
    thread::sleep(crate::VISIBLE_AFTER);

    let held_host = File::open(bob.join(HOST))?;
    let held_sessions = (0..TRIALS)
        .map(|trial| File::open(bob.join(HOST).join(format!("attack-{trial}"))))
        .collect::<std::io::Result<Vec<_>>>()?;
    lab.start_agents("c", "cat")?;
    let control = bob.join(HOST).join("allowed");
    fs::create_dir(&control)?;
    let mut prompt = File::create(control.join("prompt"))?;
    writeln!(prompt, "allowed")?;
    prompt.sync_all()?;
    wait_done(&control)?;
    assert_eq!(fs::read_to_string(control.join("output"))?, "allowed\n");

    set_acl(&lab, "", None)?;
    for trial in 0..TRIALS {
        set_acl(&lab, &format!("{HOST}/attack-{trial}"), Some(Perm::Write))?;
    }

    let sibling = format!("/proc/self/fd/{}/control", held_host.as_raw_fd());
    let denied = fs::create_dir(sibling).unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::PermissionDenied, "denial control");

    for (trial, held) in held_sessions.iter().enumerate() {
        let prompt = format!("/proc/self/fd/{}/prompt", held.as_raw_fd());
        let mut file = OpenOptions::new().write(true).create_new(true).open(prompt)?;
        writeln!(file, "attack {trial}")?;
        file.sync_all()?;
    }
    let mut executed = 0;
    for trial in 0..TRIALS {
        let dir = alice.join(HOST).join(format!("attack-{trial}"));
        wait_done(&dir)?;
        executed += usize::from(fs::read_to_string(dir.join("output"))? == format!("attack {trial}\n"));
    }
    assert_eq!(
        executed, 0,
        "{executed}/{TRIALS} sessions ran without write on {HOST}"
    );
    Ok(())
}

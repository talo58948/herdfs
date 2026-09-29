//! Round 30: swapping a claimed agents session directory.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

const HOST: &str = ".agents/c";
const TRIALS: usize = 3;

fn write_prompt(dir: &Path) -> Result<()> {
    fs::create_dir(dir)?;
    let mut prompt = File::create(dir.join("prompt"))?;
    writeln!(prompt, "run")?;
    prompt.sync_all()?;
    Ok(())
}

fn wait_for(path: &Path, wanted: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match fs::read_to_string(path) {
            Ok(text) if text.trim() == wanted => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            _ => {}
        }
        ensure!(
            Instant::now() < deadline,
            "{} never became {wanted:?}",
            path.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Replacing a claimed session directory with a symlink must not make the runner write its final
/// status into the host filesystem outside the public mount.
// contract: A1
#[test]
#[ignore = "ROUND30: a claimed session can redirect final status outside the mount"]
fn a_claimed_session_cannot_redirect_status_outside_the_mount() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("c", "carol", &[], Profile::Lan, Strategy::Stream);
    let mut lab = Lab::start(config)?;
    let mount = lab.mount_path("a").to_owned();
    fs::create_dir(mount.join(".agents"))?;
    fs::create_dir(mount.join(HOST))?;
    lab.start_agents("c", "sleep 2")?;

    let control = mount.join(HOST).join("control");
    write_prompt(&control)?;
    wait_for(&control.join("status"), "done")?;

    let mut redirected = 0;
    for trial in 0..TRIALS {
        let session = mount.join(HOST).join(format!("attack-{trial}"));
        let moved = mount.join(HOST).join(format!("moved-{trial}"));
        let outside = lab.client_dir("c").join(format!("outside-{trial}"));
        fs::create_dir(&outside)?;
        write_prompt(&session)?;
        wait_for(&session.join("status"), "running")?;
        fs::rename(&session, &moved)?;
        symlink(&outside, &session)?;
        wait_for(&outside.join("status"), "done")?;
        redirected += usize::from(fs::read_to_string(moved.join("status"))?.trim() == "running");
    }

    assert_eq!(
        redirected, 0,
        "{redirected}/{TRIALS} final statuses were written outside the mount"
    );
    Ok(())
}

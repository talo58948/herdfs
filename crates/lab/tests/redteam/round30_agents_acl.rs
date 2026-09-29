//! Round 30: agents session-directory grants.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, SPACE, Strategy};

const HOST: &str = ".agents/c";
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

/// Write on one session directory permits spawning there, but nowhere else under the host.
// contract: A1
#[test]
fn write_on_a_session_directory_allows_spawning_only_there() -> Result<()> {
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
    let allowed = alice.join(HOST).join("allowed");
    let denied = alice.join(HOST).join("denied");
    fs::create_dir(&allowed)?;
    fs::create_dir(&denied)?;
    thread::sleep(crate::VISIBLE_AFTER);

    set_acl(&lab, "", Some(Perm::Read))?;
    set_acl(&lab, &format!("{HOST}/allowed"), Some(Perm::Write))?;
    lab.start_agents("c", "cat")?;

    let refused = fs::create_dir(bob.join(HOST).join("new")).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::PermissionDenied);
    let refused = File::create(bob.join(HOST).join("denied/prompt")).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::PermissionDenied);

    let mut prompt = File::create(bob.join(HOST).join("allowed/prompt"))?;
    writeln!(prompt, "allowed")?;
    prompt.sync_all()?;
    wait_done(&allowed)?;
    assert_eq!(fs::read_to_string(allowed.join("output"))?, "allowed\n");
    Ok(())
}

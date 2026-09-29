//! U9: the agents runner on client C, with sessions requested from other clients.

use std::fs;
use std::io::ErrorKind;
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use herdfs_core::proto::{Perm, Request, Response};
use herdfs_lab::{ADMIN, LabConfig, Profile, SPACE, Strategy};

use crate::support::{self, VISIBLE_AFTER, errno, mkdir, path, save, set_acl, wait_until};

const HOST_DIR: &str = ".agents/c";
/// U9's 5 s for the session to run, then T_vis for its requester to see it end.
const SESSION_DONE: Duration = Duration::from_secs(5).saturating_add(VISIBLE_AFTER);
/// Several of the runner's 1 s polls.
const QUIET: Duration = Duration::from_secs(4);

#[test]
fn write_on_the_host_dir_is_what_spawns_a_session() -> Result<()> {
    // contract: A1
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
        .client("c", "carol", &[], Profile::Lan, Strategy::Stream);
    let Some(mut lab) = support::start(config)? else {
        return Ok(());
    };
    for dir in [".agents", HOST_DIR] {
        mkdir(&lab, dir)?;
    }
    for user in ["user:alice", "user:bob"] {
        set_acl(&lab, "", user, Some(Perm::Read))?;
    }
    set_acl(&lab, HOST_DIR, "user:alice", Some(Perm::Write))?;
    lab.start_agents("c", "cat; echo ok")?;

    let session = lab.mount_path("a").join(HOST_DIR).join("t1");
    fs::create_dir(&session)?;
    save(&session.join("prompt"), b"hello\n")?;
    let status = || match fs::read_to_string(session.join("status")) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        status => Ok(status?.trim_end() == "done"),
    };
    let took = wait_until("session t1 is done", SESSION_DONE, status)?;
    eprintln!("u9: session done, as seen from A, {took:?} after its prompt");
    assert_eq!(fs::read_to_string(session.join("output"))?, "hello\nok\n");

    let denied = lab.mount_path("b").join(HOST_DIR).join("t2");
    assert_eq!(errno(fs::create_dir(&denied)), Some(libc::EACCES));
    thread::sleep(QUIET);
    let Response::Entry { entry } = lab.rpc(
        ADMIN,
        &Request::Stat {
            space: SPACE.into(),
            path: path(HOST_DIR)?,
        },
    )?
    else {
        bail!("Stat answered something else");
    };
    let list = Request::List {
        space: SPACE.into(),
        entry: entry.id,
    };
    let Response::Entries { entries } = lab.rpc(ADMIN, &list)? else {
        bail!("List answered something else");
    };
    let sessions: Vec<_> = entries.iter().map(|entry| entry.path.name()).collect();
    assert_eq!(sessions, ["t1"]);
    Ok(())
}

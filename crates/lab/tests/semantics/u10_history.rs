//! U10: a file with 20 versions, and restoring an old one.

use std::fs;
use std::thread;

use anyhow::{Result, bail};
use herdfs_core::proto::{Request, Response};
use herdfs_lab::{LabConfig, Profile, SPACE, Strategy};

use crate::support::{self, VISIBLE_AFTER, history, path, save, version_bytes};

const FILE: &str = "notes.txt";
const VERSIONS: usize = 20;
/// The version to restore, counting from 1.
const RESTORED: usize = 5;

#[test]
fn restoring_v5_makes_it_head_and_keeps_every_version_readable() -> Result<()> {
    // contract: C14
    let Some(lab) = support::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Wan, Strategy::Fetch),
    )?
    else {
        return Ok(());
    };
    let mut saved: Vec<String> = (1..=VERSIONS).map(|n| format!("version {n}\n")).collect();
    for text in &saved {
        save(&lab.mount_path("a").join(FILE), text.as_bytes())?;
    }
    let versions = history(&lab, "alice", FILE)?;
    assert_eq!(versions.len(), VERSIONS);

    let restore = Request::Restore {
        space: SPACE.into(),
        path: path(FILE)?,
        version: versions[RESTORED - 1].id,
    };
    let Response::Version { version: head } = lab.rpc("alice", &restore)? else {
        bail!("Restore answered something else");
    };
    saved.push(saved[RESTORED - 1].clone());
    thread::sleep(VISIBLE_AFTER);
    assert_eq!(
        fs::read_to_string(lab.mount_path("b").join(FILE))?,
        saved[RESTORED - 1]
    );

    let versions = history(&lab, "bob", FILE)?;
    assert_eq!(versions.len(), VERSIONS + 1);
    assert_eq!(versions.last(), Some(&head));
    for (version, text) in versions.iter().zip(&saved) {
        assert_eq!(version_bytes(&lab, "b", "bob", version)?, text.as_bytes());
    }
    Ok(())
}

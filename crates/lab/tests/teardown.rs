//! Tear-down, in its own test binary so no other lab's processes are children of this one.
//! contract: none; this checks the lab itself.

use std::{fs, process};

use anyhow::Result;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

#[test]
fn dropping_the_lab_leaves_no_child_processes() -> Result<()> {
    let config = LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream);
    let lab = Lab::start_infra(config)?;
    assert!(!children()?.is_empty(), "the lab runs MinIO and toxiproxy");
    drop(lab);
    assert_eq!(children()?, Vec::<String>::new());
    Ok(())
}

/// `pid (command)` of every child of this process.
fn children() -> Result<Vec<String>> {
    let me = process::id().to_string();
    let mut children = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let Ok(stat) = fs::read_to_string(entry?.path().join("stat")) else {
            continue;
        };
        // After the command name come the state and then the parent's pid.
        let Some((process, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        if rest.split(' ').nth(1) == Some(me.as_str()) {
            children.push(format!("{process})"));
        }
    }
    Ok(children)
}

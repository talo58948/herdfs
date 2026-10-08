//! Tear-down, in its own test binary so no other lab's processes are children of this one.
//! contract: none; this checks the lab itself.

use std::process;

use anyhow::Result;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy, child_processes};

#[test]
fn dropping_the_lab_leaves_no_child_processes() -> Result<()> {
    let config = LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream);
    let lab = Lab::start_infra(config)?;
    assert!(!child_processes(process::id())?.is_empty(), "the lab runs MinIO and toxiproxy");
    drop(lab);
    assert_eq!(child_processes(process::id())?, Vec::<String>::new());
    Ok(())
}

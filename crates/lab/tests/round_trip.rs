//! The full lab: two clients over the authority. Skips until the binaries are built.

use std::fs::{self, File};
use std::io::Write;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy, canary};

/// T_vis plus a margin for the test's own scheduling.
const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);

#[test]
fn a_write_on_one_client_reads_back_on_another() -> Result<()> {
    // contract: C1, C2
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Wan, Strategy::Fetch),
    )?;
    let text = canary();
    let mut file = File::create(lab.mount_path("a").join("hello.txt"))?;
    file.write_all(text.as_bytes())?;
    // fsync returns once the authority has committed the version.
    file.sync_all()?;
    drop(file);
    assert_eq!(fs::read_to_string(lab.mount_path("a").join("hello.txt"))?, text);
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        assert!(lab.stats("a")?["commits"].as_u64() >= Some(1));
    }

    thread::sleep(VISIBLE_AFTER);
    assert_eq!(fs::read_to_string(lab.mount_path("b").join("hello.txt"))?, text);

    let mounts = ["a", "b"].map(|name| lab.mount_path(name).display().to_string());
    drop(lab);
    let table = fs::read_to_string("/proc/self/mountinfo")?;
    assert!(
        mounts.iter().all(|mount| !table.contains(mount.as_str())),
        "a mount outlived the lab"
    );
    Ok(())
}

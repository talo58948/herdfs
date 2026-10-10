//! Round 73: crashed mount registrations must not duplicate a restarted mount.

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy, herdfs_command};

fn status(path: Option<&std::path::Path>) -> Result<std::process::Output> {
    let mut command = herdfs_command()?;
    command.args(["--json", "status"]);
    if let Some(path) = path {
        command.arg(path);
    }
    Ok(command.output()?)
}

/// A first pathless status and explicit-path statuses control three crash/restart cycles. There is
/// still only one running mount after each restart, so stale registrations must not make it ambiguous.
// contract: U1
#[test]
fn round73_pathless_status_survives_mount_restarts() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let mut lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let initial = status(None)?;
    ensure!(
        initial.status.success(),
        "initial pathless status failed: {}",
        String::from_utf8_lossy(&initial.stderr)
    );

    let mut failures = 0;
    for trial in 0..3 {
        lab.restart_client("a")?;
        let explicit = status(Some(lab.mount_path("a")))?;
        ensure!(
            explicit.status.success(),
            "trial {trial} explicit-path control failed: {}",
            String::from_utf8_lossy(&explicit.stderr)
        );
        let pathless = status(None)?;
        let stderr = String::from_utf8_lossy(&pathless.stderr);
        eprintln!(
            "trial {trial}: pathless status succeeded={}, stderr={stderr}",
            pathless.status.success()
        );
        failures += usize::from(!pathless.status.success());
    }
    assert_eq!(
        failures, 0,
        "pathless statuses made ambiguous by stale registrations, of 3"
    );
    Ok(())
}

//! Platform mount-table and forced-unmount operations used by the lab.

use std::path::Path;
use std::process::Command;

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{Result, ensure};

/// Whether `path` is present in the host mount table.
pub fn mounted(path: &Path) -> bool {
    mount_points().is_ok_and(|mounts| mounts.iter().any(|mount| mount == path))
}

#[cfg(target_os = "linux")]
fn mount_points() -> Result<Vec<std::path::PathBuf>> {
    let table = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(table
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|path| path.replace("\\040", " ").into())
        .collect())
}

#[cfg(target_os = "macos")]
fn mount_points() -> Result<Vec<std::path::PathBuf>> {
    let output = super::process::run(&mut Command::new("mount"))?;
    Ok(output
        .lines()
        .filter_map(|line| {
            line.split_once(" on ")?
                .1
                .split_once(" (")
                .map(|(path, _)| path.into())
        })
        .collect())
}

/// Force-detaches `path` after its mount process has stopped.
pub(crate) fn detach(path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    let status = Command::new("fusermount3")
        .args(["-u", "-z"])
        .arg(path)
        .status()?;
    #[cfg(target_os = "macos")]
    let status = Command::new("umount").arg("-f").arg(path).status()?;
    ensure!(status.success(), "forced unmount of {}: {status}", path.display());
    Ok(())
}

/// Force-detaches a loopback NFS mount after its server has stopped.
pub(crate) fn detach_nfs(path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    let status = Command::new("sudo").args(["umount", "-f"]).arg(path).status()?;
    #[cfg(target_os = "macos")]
    let status = Command::new("umount").arg("-f").arg(path).status()?;
    ensure!(status.success(), "forced unmount of {}: {status}", path.display());
    Ok(())
}

/// Aborts a stuck mount so blocked kernel requests return.
pub fn abort_mount(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    return detach(path);

    #[cfg(target_os = "linux")]
    {
        let table = std::fs::read_to_string("/proc/self/mountinfo")?;
        let minor = table
            .lines()
            .map(|line| line.split(' ').collect::<Vec<_>>())
            .find(|fields| fields.get(4).copied() == path.to_str())
            .and_then(|fields| Some(fields.get(2)?.split_once(':')?.1.to_owned()))
            .with_context(|| format!("{} is not mounted", path.display()))?;
        std::fs::write(format!("/sys/fs/fuse/connections/{minor}/abort"), "1")?;
        Ok(())
    }
}

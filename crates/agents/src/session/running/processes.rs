//! Platform process inspection used when a recorded group leader has exited.

use std::io;

#[cfg(target_os = "linux")]
use std::fs;

use super::invalid;
use crate::session::process::kill_process;

const SESSION_ID: &str = "HERDFS_SESSION_ID";

fn marked(environ: &[u8], session_id: &str) -> bool {
    let marker = format!("{SESSION_ID}={session_id}");
    environ
        .split(|byte| *byte == 0)
        .any(|variable| variable == marker.as_bytes())
}

#[cfg(target_os = "linux")]
pub(super) fn start_time(pid: u32) -> io::Result<String> {
    let (group, started) = linux_identity(pid)?;
    Ok(format!("{started}:{group}"))
}

#[cfg(target_os = "linux")]
pub(super) fn kill_members(group: u32, session_id: &str) -> io::Result<()> {
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok((member_group, _)) = linux_identity(pid) else {
            continue;
        };
        if member_group != group {
            continue;
        }
        let environ = match fs::read(format!("/proc/{pid}/environ")) {
            Ok(environ) => environ,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if marked(&environ, session_id) {
            kill_process(pid)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_identity(pid: u32) -> io::Result<(u32, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .ok_or_else(|| invalid("/proc process stat"))?;
    let fields = fields.split_whitespace();
    let group = fields
        .clone()
        .nth(2)
        .ok_or_else(|| invalid("/proc process group"))?;
    let started = fields
        .clone()
        .nth(19)
        .ok_or_else(|| invalid("/proc process start time"))?;
    let group = group.parse().map_err(|_| invalid("/proc process group"))?;
    Ok((group, started.to_owned()))
}

#[cfg(target_os = "macos")]
pub(super) fn kill_members(group: u32, session_id: &str) -> io::Result<()> {
    for pid in group_members(group)? {
        let environ = match process_environment(pid) {
            Ok(environ) => environ,
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => continue,
            Err(err) => return Err(err),
        };
        if marked(&environ, session_id) {
            kill_process(pid)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[expect(unsafe_code, reason = "libproc lists the members of a process group on macOS")]
fn group_members(group: u32) -> io::Result<Vec<u32>> {
    let count = unsafe { libc::proc_listpgrppids(group as i32, std::ptr::null_mut(), 0) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pids = vec![0_i32; count as usize + 16];
    let capacity =
        i32::try_from(pids.len() * size_of::<libc::pid_t>()).map_err(|_| invalid("process group size"))?;
    let bytes = unsafe { libc::proc_listpgrppids(group as i32, pids.as_mut_ptr().cast(), capacity) };
    if bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(bytes as usize / size_of::<libc::pid_t>());
    Ok(pids.into_iter().filter_map(|pid| pid.try_into().ok()).collect())
}

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "KERN_PROCARGS2 exposes same-user process environments on macOS"
)]
fn process_environment(pid: u32) -> io::Result<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
    let mut length = 0;
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut environ = vec![0; length];
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            environ.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    environ.truncate(length);
    Ok(environ)
}

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "proc_pidinfo provides precise process identity on macOS"
)]
pub(super) fn start_time(pid: u32) -> io::Result<String> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: proc_pidinfo receives a correctly sized, writable PROC_PIDTBSDINFO buffer.
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a full buffer was initialized by proc_pidinfo.
    let info = unsafe { info.assume_init() };
    Ok(format!(
        "{}:{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec, info.pbi_pgid
    ))
}

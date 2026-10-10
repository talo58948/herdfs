//! Platform identities that keep process-group recovery within the recorded boot.

use std::io;

#[cfg(target_os = "linux")]
use std::fs;

use super::invalid;

#[cfg(target_os = "linux")]
pub(super) fn boot_id() -> io::Result<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|id| id.trim().to_owned())
}

#[cfg(target_os = "linux")]
pub(super) fn start_time(pid: u32) -> io::Result<Option<String>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let (_, fields) = stat
        .rsplit_once(") ")
        .ok_or_else(|| invalid("/proc process stat"))?;
    let started = fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| invalid("/proc process start time"))?;
    Ok(Some(started.to_owned()))
}

#[cfg(target_os = "macos")]
#[expect(unsafe_code, reason = "KERN_BOOTTIME identifies the current macOS boot")]
pub(super) fn boot_id() -> io::Result<String> {
    let mut boot = std::mem::MaybeUninit::<libc::timeval>::uninit();
    let mut length = size_of::<libc::timeval>();
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    // SAFETY: sysctl receives a correctly sized, writable timeval buffer.
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            boot.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length != size_of::<libc::timeval>() {
        return Err(invalid("boot time"));
    }
    // SAFETY: a full buffer was initialized by sysctl.
    let boot = unsafe { boot.assume_init() };
    Ok(format!("{}:{}", boot.tv_sec, boot.tv_usec))
}

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "proc_pidinfo provides precise process identity on macOS"
)]
pub(super) fn start_time(pid: u32) -> io::Result<Option<String>> {
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
        let err = io::Error::last_os_error();
        return if read == 0 && err.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(err)
        };
    }
    // SAFETY: a full buffer was initialized by proc_pidinfo.
    let info = unsafe { info.assume_init() };
    Ok(Some(format!(
        "{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    )))
}

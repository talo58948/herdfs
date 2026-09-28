//! Direct-I/O checks for behavior that still goes through the kernel's mapped pages.

use std::fs::{self, File, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

use crate::save;

const PAGE: usize = 4096;

// contract: C1
#[test]
#[expect(unsafe_code, reason = "shared mmap is the behavior under test")]
fn direct_io_keeps_one_clients_mappings_coherent_and_executable() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let path = lab.mount_path("a").join("mapped");
    save(&path, &vec![b'0'; PAGE])?;
    let mapped = File::options().read(true).write(true).open(&path)?;
    let mut other = File::options().read(true).write(true).open(&path)?;

    with_shared_mapping(&mapped, PAGE, |map| {
        // SAFETY: `map` points to the writable mapping for at least four bytes.
        unsafe { std::slice::from_raw_parts_mut(map, 4) }.copy_from_slice(b"map!");
        sync(map, PAGE, libc::MS_SYNC)?;
        let mut read = [0; 4];
        other.read_exact(&mut read)?;
        assert_eq!(&read, b"map!", "read through another descriptor");

        other.seek(SeekFrom::Start(4))?;
        other.write_all(b"write!")?;
        sync(map, PAGE, libc::MS_SYNC | libc::MS_INVALIDATE)?;
        // SAFETY: `map` still points to the live mapping, and bytes 4..10 lie inside it.
        let seen = unsafe { std::slice::from_raw_parts(map.add(4), 6) };
        assert_eq!(seen, b"write!", "mapping after descriptor write");
        Ok(())
    })?;

    let executable = lab.mount_path("a").join("copied-true");
    fs::copy("/bin/true", &executable)?;
    fs::set_permissions(&executable, Permissions::from_mode(0o755))?;
    ensure!(
        Command::new(executable).status()?.success(),
        "copied binary failed"
    );
    Ok(())
}

#[expect(unsafe_code, reason = "shared mmap is the behavior under test")]
fn with_shared_mapping(
    file: &File,
    len: usize,
    use_mapping: impl FnOnce(*mut u8) -> io::Result<()>,
) -> io::Result<()> {
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    // SAFETY: the descriptor stays open and the file has at least `len` bytes.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            prot,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let result = use_mapping(map.cast());
    // SAFETY: unmaps exactly the mapping made above, after its last use.
    unsafe { libc::munmap(map, len) };
    result
}

#[expect(unsafe_code, reason = "msync is the behavior under test")]
fn sync(map: *mut u8, len: usize, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: `map` is the live mapping of `len` bytes passed to `with_shared_mapping`.
    if unsafe { libc::msync(map.cast(), len, flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

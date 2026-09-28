//! Round 16: a file that one client mapped, opened and mapped again after another client
//! rewrote it with bytes of the same length. Every open uses direct I/O, but a mapping still
//! reads through the kernel's page cache, which the first mapping filled.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::{VISIBLE_AFTER, random_bytes, read_at, save, two_clients};

const LEN: usize = 1 << 20;
const TRIALS: usize = 3;
/// Far longer than a mapped read of `LEN` bytes over LAN links takes.
const HUNG_AFTER: Duration = Duration::from_secs(120);

/// All `len` bytes of `file` through a shared read-only mapping.
#[expect(unsafe_code, reason = "reading through mmap is the behavior under test")]
fn read_mapped(file: &File, len: usize) -> std::io::Result<Vec<u8>> {
    let (prot, flags, fd) = (libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd());
    // SAFETY: a new read-only mapping of an open file; nothing else aliases it.
    let map = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd, 0) };
    if map == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the `len` bytes mapped above stay mapped until munmap.
    let bytes = unsafe { std::slice::from_raw_parts(map.cast::<u8>(), len).to_vec() };
    // SAFETY: unmaps exactly the mapping made above, after its last use.
    unsafe { libc::munmap(map, len) };
    Ok(bytes)
}

/// Aborts the FUSE connection of the mount at `mount`, so a client stuck in the kernel exits.
fn abort_connection(mount: &Path) -> Result<()> {
    let table = fs::read_to_string("/proc/self/mountinfo")?;
    let minor = table
        .lines()
        .map(|line| line.split(' ').collect::<Vec<_>>())
        .find(|fields| fields.get(4).copied() == mount.to_str())
        .and_then(|fields| Some(fields.get(2)?.split_once(':')?.1.to_owned()))
        .with_context(|| format!("{} is not mounted", mount.display()))?;
    fs::write(format!("/sys/fs/fuse/connections/{minor}/abort"), "1")?;
    Ok(())
}

/// In each trial, a saves v1 of a new file; after T_vis b opens it and reads it whole, through a
/// mapping if `mapped_first`, else with `read`, then closes it. a saves v2, other bytes of the
/// same length; after T_vis b opens the file again and reads it whole through a mapping. Returns
/// what that last read gave per trial: `v2`, `v1` or `neither`.
fn remap_after_a_rewrite(mapped_first: bool) -> Result<Option<Vec<&'static str>>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let mount = lab.mount_path("b").to_owned();
    let (done, finished) = mpsc::channel::<()>();
    let watchdog = thread::spawn(move || {
        if finished.recv_timeout(HUNG_AFTER).is_err() {
            let _ = abort_connection(&mount);
        }
    });
    let mut seen = Vec::new();
    for trial in 0..TRIALS {
        let name = format!("f{trial}");
        let (old, new) = (random_bytes(LEN), random_bytes(LEN));
        save(&lab.mount_path("a").join(&name), &old)?;
        thread::sleep(VISIBLE_AFTER);
        let path = lab.mount_path("b").join(&name);
        let first = File::open(&path)?;
        let first_read = match mapped_first {
            true => read_mapped(&first, LEN)?,
            false => read_at(&first, 0, LEN)?,
        };
        assert!(first_read == old, "b's first read gives v1");
        drop(first);

        save(&lab.mount_path("a").join(&name), &new)?;
        thread::sleep(VISIBLE_AFTER);
        let bytes = read_mapped(&File::open(&path)?, LEN)?;
        seen.push(match bytes {
            bytes if bytes == new => "v2",
            bytes if bytes == old => "v1",
            _ => "neither",
        });
    }
    let _ = done.send(());
    let _ = watchdog.join();
    Ok(Some(seen))
}

/// Repro: [`remap_after_a_rewrite`] with b's first read through a mapping. That mapping leaves
/// v1's pages in b's page cache. The rewrite keeps the size, and b's attribute refresh and open
/// drop no pages, so b's mapping of an open made T_vis after a's commit reads v1 (C2). The
/// control is [`a_file_read_then_remapped_after_a_rewrite_maps_the_new_version`].
// contract: C2
#[test]
fn a_file_mapped_then_remapped_after_a_rewrite_maps_the_new_version() -> Result<()> {
    let Some(seen) = remap_after_a_rewrite(true)? else {
        return Ok(());
    };
    assert_eq!(seen, ["v2"; TRIALS], "what b's later mapping reads, per trial");
    Ok(())
}

/// Control: b's first read goes through `read`, which direct I/O keeps out of the page cache.
// contract: C2
#[test]
fn a_file_read_then_remapped_after_a_rewrite_maps_the_new_version() -> Result<()> {
    let Some(seen) = remap_after_a_rewrite(false)? else {
        return Ok(());
    };
    assert_eq!(seen, ["v2"; TRIALS], "what b's later mapping reads, per trial");
    Ok(())
}

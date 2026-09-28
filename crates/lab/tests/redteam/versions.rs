//! Round 2: what a handle open on one version reads while another client commits or removes.

use std::fs::{self, File};
use std::io::{self, Read, Seek};
use std::os::fd::AsRawFd;
use std::thread;

use anyhow::Result;

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, read_at, save, two_clients};

const LEN: usize = 1 << 20;
const PAGE: usize = 4096;

/// Repro: on b, open a 2 MiB `f` and read its first page; a commits a 1 MiB `f`; after T_vis b
/// stats `f`, then reads the open handle to its end. The stat gives the kernel the new version's
/// size, getattr ignoring which version the handle reads, and the kernel ends the handle's reads
/// there: b gets the first half of the version it opened.
// contract: C3
#[test]
#[ignore = "REDTEAM2: a stat cuts an open handle's version short at the new head's size"]
fn a_reader_gets_all_of_its_version_after_a_shorter_one_is_committed() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (old, new) = (vec![b'1'; 2 * LEN], vec![b'2'; LEN]);
    save(&lab.mount_path("a").join("f"), &old)?;
    thread::sleep(VISIBLE_AFTER);
    let path = lab.mount_path("b").join("f");
    let mut file = File::open(&path)?;
    assert_bytes(&read_at(&file, 0, PAGE)?, &old[..PAGE], "the first page");

    save(&lab.mount_path("a").join("f"), &new)?;
    thread::sleep(VISIBLE_AFTER);
    assert_eq!(
        fs::metadata(&path)?.len(),
        new.len() as u64,
        "b's stat after T_vis"
    );
    let mut read = Vec::new();
    file.rewind()?;
    file.read_to_end(&mut read)?;
    assert_bytes(&read, &old, "the handle opened on the 2 MiB version");
    Ok(())
}

/// Repro: on b, open `f` at v1 and read its first page; a commits v2; after T_vis b opens `f`
/// again and maps it, reads the middle through the first handle, then through the mapping. The
/// second handle is direct I/O, but a mapping of it goes through the page cache, which the first
/// handle's read has just filled with v1.
// contract: C2, C3
#[test]
#[ignore = "REDTEAM2: a mapping of a direct handle reads the page cache another version filled"]
fn a_mapping_opened_after_a_commit_never_reads_the_older_version() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (old, new) = (vec![b'1'; LEN], vec![b'2'; LEN]);
    save(&lab.mount_path("a").join("f"), &old)?;
    thread::sleep(VISIBLE_AFTER);
    let path = lab.mount_path("b").join("f");
    let early = File::open(&path)?;
    assert_bytes(&read_at(&early, 0, PAGE)?, &old[..PAGE], "the first open");

    save(&lab.mount_path("a").join("f"), &new)?;
    thread::sleep(VISIBLE_AFTER);
    let late = File::open(&path)?;
    let middle = LEN / 2;
    let mapped = read_mapped(&late, LEN, middle..middle + PAGE, || {
        read_at(&early, middle as u64, PAGE).map(drop)
    })?;
    let version = match &mapped[..] {
        bytes if bytes == &new[..PAGE] => "v2",
        bytes if bytes == &old[..PAGE] => "v1",
        _ => "neither version",
    };
    assert_eq!(version, "v2", "what the mapping of the later open reads");
    Ok(())
}

/// Bytes `range` of `file` through a shared read-only mapping of its first `size` bytes, read
/// only after `between` has run.
#[expect(unsafe_code, reason = "reading through mmap is the behavior under test")]
fn read_mapped(
    file: &File,
    size: usize,
    range: std::ops::Range<usize>,
    between: impl FnOnce() -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    assert!(range.end <= size, "the range lies inside the mapping");
    let (prot, flags, fd) = (libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd());
    // SAFETY: a new read-only mapping of an open file; nothing else aliases it.
    let map = unsafe { libc::mmap(std::ptr::null_mut(), size, prot, flags, fd, 0) };
    if map == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `range` lies inside the `size` bytes mapped above, which stay mapped until munmap.
    let read = between().map(|()| unsafe {
        std::slice::from_raw_parts(map.cast::<u8>().add(range.start), range.len()).to_vec()
    });
    // SAFETY: unmaps exactly the mapping made above, after its last use.
    unsafe { libc::munmap(map, size) };
    read
}

/// Repro: on b, open a 12 MiB `f` and read its first page; a removes `f`; b reads the end through
/// the same handle. Remove deletes the entry's versions with it, so the grant for the end's
/// chunks fails, and a read of a file that b opened while it existed breaks off halfway.
// contract: C3 (docs/architecture.md: of files unlinked while open, only writing is out of v0)
#[test]
#[ignore = "REDTEAM2: removing a file breaks reads through handles already open on it"]
fn an_open_file_stays_readable_after_another_client_removes_it() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let bytes = random_bytes(12 << 20);
    save(&lab.mount_path("a").join("f"), &bytes)?;
    thread::sleep(VISIBLE_AFTER);
    let file = File::open(lab.mount_path("b").join("f"))?;
    assert_bytes(&read_at(&file, 0, PAGE)?, &bytes[..PAGE], "the start");

    fs::remove_file(lab.mount_path("a").join("f"))?;
    let tail = bytes.len() - PAGE;
    let read = read_at(&file, tail as u64, PAGE);
    assert!(read.is_ok(), "reading the end after the remove: {read:?}");
    assert_bytes(&read?, &bytes[tail..], "the end");
    Ok(())
}

//! Round 5: a file another client replaces the way rsync, `sed -i` and git save files: a
//! temporary file written beside it, closed, and renamed over it.

use std::fs;
use std::process::Command;
use std::thread;

use anyhow::Result;
use herdfs_lab::run;

use crate::{VISIBLE_AFTER, random_bytes, save, two_clients};

const LEN: usize = 8 << 20;

/// Repro: a saves an 8 MiB `data`; b copies a new `data`, one byte longer, over it with rsync,
/// which writes a temporary file beside it, closes it without fsync and renames it over `data`;
/// a reads `data` as soon as rsync returns. The rename reaches the authority when b makes it, but
/// the temporary file's bytes only when b's committer commits them, 250 ms after the close and an
/// upload later (journal.rs `QUIET`, commit.rs `attempt`). Until then `data` names an entry with
/// no version, and a reads 0 bytes: neither the file it replaced nor the new one. Git replaces its
/// index the same way, so `git add` on b makes `git status` on a fail with "index file smaller
/// than expected". If b fsyncs the temporary file before its rename, a reads the new bytes.
// contract: C7, C3 (a rename replaces a file whole; a read never returns a version not committed)
#[test]
fn a_file_replaced_by_rename_reads_whole_on_another_client() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (old, new) = (random_bytes(LEN), random_bytes(LEN + 1));
    save(&lab.mount_path("a").join("data"), &old)?;
    let source = tempfile::NamedTempFile::new()?;
    fs::write(source.path(), &new)?;
    thread::sleep(VISIBLE_AFTER);
    let copy = lab.mount_path("b").join("data");
    run(Command::new("rsync").arg(source.path()).arg(&copy))?;
    let read = fs::read(lab.mount_path("a").join("data"))?;
    assert!(
        read == old || read == new,
        "a read {} bytes, neither the old {} nor the new {}",
        read.len(),
        old.len(),
        new.len()
    );
    Ok(())
}

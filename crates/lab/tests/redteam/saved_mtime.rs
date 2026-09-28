//! Round 6: the modification time a program sees right after it saves a file, against the one
//! the file keeps.

use std::fs;
use std::thread;

use anyhow::Result;

use crate::{VISIBLE_AFTER, two_clients};

/// Repro: a writes `notes` and closes it without fsync, as an editor with 'nofsync' saves, and
/// stats it at once; once the write has committed, a stats it again. Right after the close the
/// client reports the journal copy's mtime, to the nanosecond (fs/mod.rs `attr`); the commit
/// keeps only milliseconds of it (journal.rs `seal`, `unix_ms`), so the file's mtime then moves
/// back by the part below a millisecond though nobody wrote it. vim with 'nofsync' remembers the
/// mtime after `:w`, and its next `:w` asks "WARNING: The file has been changed since reading
/// it!!! Do you really want to write to it (y/n)?"; on a local directory it writes.
// contract: none (docs/architecture.md: mtime is stored and reported; only atime and ctime are out of v0)
#[test]
#[ignore = "REDTEAM6: a saved file's mtime changes when its write commits, so editors warn it changed on disk"]
fn a_saved_files_mtime_stays_put_when_it_commits() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let path = lab.mount_path("a").join("notes");
    fs::write(&path, "first draft\n")?;
    let saved = fs::metadata(&path)?.modified()?;
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    let later = fs::metadata(&path)?.modified()?;
    assert_eq!(
        later, saved,
        "a's mtime of `notes` after its commit, against right after its save"
    );
    Ok(())
}

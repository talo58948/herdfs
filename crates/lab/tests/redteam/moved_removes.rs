//! Round 11: a remove by name on a client whose cached entry there has just been moved away by
//! another client, while the moved entry holds a closed write still waiting for its commit.

use std::fs;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::Request;
use herdfs_lab::{ADMIN, SPACE};

use crate::{VISIBLE_AFTER, save, two_clients};

/// a saves `p`, then rewrites it and closes without fsync, so the rewrite waits out the
/// committer's quiet period. The admin moves `p` to `q` and creates a new, empty `p`, as an
/// atomic save or a log rotation elsewhere does. With `remove`, a then removes `p`, which the
/// authority now resolves to the new file. Once a has settled every commit and T_vis has
/// passed, returns what b reads at `q`, the entry a rewrote.
fn moved_entry_after_a_remove(remove: bool) -> Result<Option<Vec<u8>>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let on_a = lab.mount_path("a").join("p");
    save(&on_a, b"saved")?;
    fs::write(&on_a, b"rewritten")?;
    let (p, q) = (SpacePath::try_from("p")?, SpacePath::try_from("q")?);
    let moved = Request::Rename {
        space: SPACE.into(),
        from: p.clone(),
        to: q,
    };
    lab.rpc(ADMIN, &moved)?;
    let created = Request::Create {
        space: SPACE.into(),
        path: p,
        mode: 0o644,
    };
    lab.rpc(ADMIN, &created)?;
    ensure!(
        lab.stats("a")?["pending_commits"] == 1,
        "a's rewrite still waits for its commit"
    );
    if remove {
        fs::remove_file(&on_a)?;
    }
    lab.wait_committed("a")?;
    std::thread::sleep(VISIBLE_AFTER);
    Ok(Some(fs::read(lab.mount_path("b").join("q"))?))
}

/// Repro: [`moved_entry_after_a_remove`] with the remove. a resolves `p` from the entry it cached
/// there within T_vis, the one now at `q`, while the authority removes the new file at `p`; a then
/// discards the journal copy of the entry it resolved (fs/namespace.rs `remove`, whose
/// `journal.discard(removed.id)` names an entry nobody removed). So a's closed rewrite of the
/// entry at `q`, which still exists, never commits, and `q` keeps the saved bytes. Deletion wins
/// only for the entry deleted; the rewrite was closed, so it commits (C4a), and C7 lets a's view
/// of the name lag within T_vis, not its writes to the entry. The control is
/// [`a_closed_write_reaches_its_moved_entry`], without the remove, where `q` holds the rewrite.
// contract: C4a, C7
#[test]
#[ignore = "ROUND11: removing a name another client reused discards the moved entry's write"]
fn a_remove_of_a_reused_name_keeps_the_moved_entrys_write() -> Result<()> {
    let Some(read) = moved_entry_after_a_remove(true)? else {
        return Ok(());
    };
    assert_eq!(String::from_utf8_lossy(&read), "rewritten", "b's read of q");
    Ok(())
}

// contract: C4a, C7
#[test]
fn a_closed_write_reaches_its_moved_entry() -> Result<()> {
    let Some(read) = moved_entry_after_a_remove(false)? else {
        return Ok(());
    };
    assert_eq!(String::from_utf8_lossy(&read), "rewritten", "b's read of q");
    Ok(())
}

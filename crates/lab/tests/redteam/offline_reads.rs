//! Round 12: reading back a saved file on its own client after that client loses its authority
//! link.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::thread;

use anyhow::Result;
use herdfs_lab::Links;

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

const SAVED: &[u8] = b"saved on a, then read back offline";

/// a creates and saves `f`, waiting for its commit. With `stat`, a stats `f` after T_vis. After
/// T_vis, so the kernel asks again, a loses its authority link and reads `f` back. Returns the
/// size a's stat then reports and the bytes a reads, once the link is back.
fn read_back_offline(stat: bool) -> Result<Option<(u64, Vec<u8>)>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let path = lab.mount_path("a").join("f");
    save(&path, SAVED)?;
    if stat {
        thread::sleep(VISIBLE_AFTER);
        fs::metadata(&path)?;
    }
    thread::sleep(VISIBLE_AFTER);
    lab.cut("a", Links::Authority)?;
    let read = fs::metadata(&path).and_then(|meta| Ok((meta.len(), fs::read(&path)?)));
    lab.restore("a", Links::Authority)?;
    Ok(Some(read?))
}

/// Repro: [`read_back_offline`] without the stat. Offline, a answers the lookup with the last
/// entry it cached at `f`'s path, however old (fs/paths.rs `held`), which is the one creating `f`
/// cached, of size 0: the commit that settled the save dropped the journal copy but left that
/// entry, and only a fetch replaces it. a's stat reports 0 bytes and its read returns none,
/// although a wrote and committed every byte and still caches them. Read-your-writes holds after
/// `close` or `fsync` (C1), and offline "the last entry seen ... keeps cached reads ... working"
/// (fs/paths.rs, C12). The control is [`an_offline_read_after_a_stat_returns_the_saved_bytes`],
/// where a stat after the commit fetched the entry and the offline read returns every byte.
// contract: C1, C12
#[test]
fn an_offline_read_returns_the_bytes_this_client_saved() -> Result<()> {
    let Some((len, read)) = read_back_offline(false)? else {
        return Ok(());
    };
    let expected = (SAVED.len() as u64, SAVED);
    assert_eq!((len, &read[..]), expected, "a's offline stat and read");
    Ok(())
}

// contract: C1, C12
#[test]
fn an_offline_read_after_a_stat_returns_the_saved_bytes() -> Result<()> {
    let Some((len, read)) = read_back_offline(true)? else {
        return Ok(());
    };
    let expected = (SAVED.len() as u64, SAVED);
    assert_eq!((len, &read[..]), expected, "a's offline stat and read");
    Ok(())
}

/// Repro: a saves `log` and, after T_vis, loses its authority link and appends a line. The lookup
/// answers with the size-0 entry creating `log` cached, as in
/// [`an_offline_read_returns_the_bytes_this_client_saved`], so the kernel places the append at
/// offset 0: the line overwrites the saved bytes instead of following them, and that is what
/// commits on reconnect. Offline writes to a file this client holds commit on reconnect (C12), and
/// read-your-writes holds after `fsync` (C1). The controls are offline_heads.rs's round-2 test,
/// which appends before T_vis, and `an_offline_append_to_a_file_this_client_saved_lands` in
/// offline_renames.rs, which stats the file after T_vis first; both commit "first\nsecond\n".
// contract: C1, C12
#[test]
fn an_offline_append_after_t_vis_follows_the_saved_bytes() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let path = lab.mount_path("a").join("log");
    save(&path, b"first\n")?;
    thread::sleep(VISIBLE_AFTER);
    lab.cut("a", Links::Authority)?;
    let appended = OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(b"second\n"));
    lab.restore("a", Links::Authority)?;
    assert!(appended.is_ok(), "a's offline append: {appended:?}");
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    let read = fs::read(lab.mount_path("b").join("log"))?;
    assert_bytes(&read, b"first\nsecond\n", "b's read of log");
    Ok(())
}

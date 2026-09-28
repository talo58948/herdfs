//! Round 13: a read of an open file that finds the version it follows hidden from its client,
//! while another writer keeps committing new versions of the file.

use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request, Version};
use herdfs_lab::{ADMIN, Lab, SPACE};

use crate::{VISIBLE_AFTER, random_bytes, read_at, save, two_clients};

/// Every version has this size, so reads stay within the size the kernel knows and reach the
/// client, which asks nothing first.
const LEN: usize = 1 << 20;
const PAGE: usize = 4096;
/// Past what the first read of page 0 makes the kernel read ahead.
const LATER: u64 = (LEN / 2) as u64;
const TRIALS: usize = 20;
/// Far longer than a read over LAN takes.
const HUNG_AFTER: Duration = Duration::from_secs(15);

/// Grants bob read on `shared` anew, so that versions committed before now are hidden from him
/// unless they are the head (S8).
fn regrant(lab: &Lab) -> Result<()> {
    for perm in [None, Some(Perm::Read)] {
        let request = Request::SetAcl {
            space: SPACE.into(),
            path: SpacePath::try_from("shared")?,
            principal: "user:bob".into(),
            perm,
        };
        lab.rpc(ADMIN, &request)?;
    }
    Ok(())
}

/// Commits `base`'s chunks again as a new head of its entry, as the admin.
fn recommit(lab: &Lab, base: &Version, request: u64) -> Result<()> {
    let commit = Request::Commit {
        space: SPACE.into(),
        entry: base.entry,
        request_id: format!("again-{request}"),
        base: Some(base.id),
        guarded: false,
        chunks: base.chunks.clone(),
        mtime_ms: None,
    };
    lab.rpc(ADMIN, &commit)?;
    Ok(())
}

/// The threads of process `pid` that wait in the kernel for a page lock.
fn waiting_for_pages(pid: u32) -> Result<usize> {
    let mut waiting = 0;
    for task in fs::read_dir(format!("/proc/{pid}/task"))? {
        let wchan = fs::read_to_string(task?.path().join("wchan")).unwrap_or_default();
        waiting += usize::from(wchan == "folio_wait_bit_common");
    }
    Ok(waiting)
}

/// Aborts the FUSE connection of the mount at `mount`, which fails the requests it holds, so that
/// a client stuck in the kernel can exit and the lab can be torn down.
fn abort_connection(mount: &Path) -> Result<()> {
    let table = fs::read_to_string("/proc/self/mountinfo")?;
    // The fifth field is the mount point; the third its device, whose minor number names the
    // connection.
    let minor = table
        .lines()
        .map(|line| line.split(' ').collect::<Vec<_>>())
        .find(|fields| fields.get(4).copied() == mount.to_str())
        .and_then(|fields| Some(fields.get(2)?.split_once(':')?.1.to_owned()))
        .with_context(|| format!("{} is not mounted", mount.display()))?;
    fs::write(format!("/sys/fs/fuse/connections/{minor}/abort"), "1")?;
    Ok(())
}

/// Bob's only access is read on `shared`. In each of 20 trials, a saves v1 of a new file there,
/// and bob is granted read on `shared` anew; b opens the file and reads its first page; the admin
/// commits v2 with v1's bytes, which hides v1 from bob. After T_vis b reads a page in the middle
/// through the same descriptor; with `busy`, the admin meanwhile keeps committing new versions
/// with the same bytes. Returns the first read that had not returned after 15 s, if any, with
/// how many of b's client threads then wait for a page lock; the test then aborts b's FUSE
/// connection, since the client cannot even be killed.
fn hung_read(busy: bool) -> Result<Option<Option<String>>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    fs::create_dir(lab.mount_path("a").join("shared"))?;
    regrant(&lab)?;
    let root = Request::SetAcl {
        space: SPACE.into(),
        path: SpacePath::root(),
        principal: "user:bob".into(),
        perm: None,
    };
    lab.rpc(ADMIN, &root)?;
    let requests = AtomicU64::new(0);
    for trial in 0..TRIALS {
        let name = format!("shared/f{trial}");
        save(&lab.mount_path("a").join(&name), &random_bytes(LEN))?;
        let v1 = lab.head(ADMIN, &name)?;
        regrant(&lab)?;
        thread::sleep(VISIBLE_AFTER);
        let file = File::open(lab.mount_path("b").join(&name))?;
        read_at(&file, 0, PAGE)?;
        recommit(&lab, &v1, requests.fetch_add(1, Relaxed))?;
        thread::sleep(VISIBLE_AFTER);

        let stop = AtomicBool::new(false);
        let returned = thread::scope(|scope| {
            scope.spawn(|| {
                while busy && !stop.load(Relaxed) {
                    let _ = recommit(&lab, &v1, requests.fetch_add(1, Relaxed));
                }
            });
            let (done, read) = mpsc::channel();
            // Detached, since a hung read returns only once the connection is aborted.
            let reader = file.try_clone()?;
            thread::spawn(move || done.send(read_at(&reader, LATER, PAGE).map(drop)));
            let returned = read.recv_timeout(HUNG_AFTER).is_ok();
            stop.store(true, Relaxed);
            anyhow::Ok(returned)
        })?;
        if !returned {
            let waiting = waiting_for_pages(lab.client_pid("b").context("b's client runs")?)?;
            abort_connection(lab.mount_path("b"))?;
            let hung = format!("trial {trial}, with {waiting} of b's client threads waiting for a page lock");
            return Ok(Some(Some(hung)));
        }
    }
    Ok(Some(None))
}

/// Repro: [`hung_read`] with the admin's commits. The read finds v1 hidden and moves the file on
/// (fs/ask.rs `usable`): it forgets the version followed, and `located` asks where the entry is,
/// since its path is older than T_vis, which follows the head (`relocate`, `follow`, `move_on`,
/// `head`) without waiting, since no version was followed; then `usable` asks for the head again.
/// When the admin has committed in between, that second `head` replaces a version known, so it
/// waits for the kernel to drop the file's pages (`Kernel::apply` with `wait`), which waits for
/// the pages this read holds until it is answered: the read never returns, and neither the client
/// nor the mount can be stopped (SIGKILL leaves the client waiting in the kernel) until the FUSE
/// connection is aborted by hand. The control is [`a_read_that_moves_an_open_file_on_returns`],
/// where the admin commits once, so both heads name the same version.
// contract: C3 (an open file follows its latest version between reads, as a local file does)
#[test]
fn a_read_that_moves_an_open_file_on_returns_while_others_commit() -> Result<()> {
    let Some(hung) = hung_read(true)? else {
        return Ok(());
    };
    assert_eq!(hung, None, "a read that had not returned after {HUNG_AFTER:?}");
    Ok(())
}

// contract: C3
#[test]
fn a_read_that_moves_an_open_file_on_returns() -> Result<()> {
    let Some(hung) = hung_read(false)? else {
        return Ok(());
    };
    assert_eq!(hung, None, "a read that had not returned after {HUNG_AFTER:?}");
    Ok(())
}

//! Round 9: a file stays open on one client while another client commits over it, and the version
//! the handle follows becomes unusable to its client: it predates the client's access and is no
//! longer the head (S8), or it leaves retention (C14).

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request, Response, VersionId};
use herdfs_lab::{ADMIN, Collection, Lab, LabConfig, Profile, SPACE, Strategy};

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, read_at, save};

/// Several chunks, so a read of the first page leaves the last one unfetched.
const LEN: usize = 12 << 20;
const PAGE: usize = 4096;
const MIDDLE: u64 = (LEN / 2) as u64;
/// Longer than a collection takes to run once `Collection::QUICK` settings prune a version.
const PRUNED_WITHIN: Duration = Duration::from_secs(10);

/// Client `a` as alice and `b` as bob, streaming over LAN links, the authority collecting with
/// `collection` or its defaults; `None` until the binaries are built.
fn start(collection: Option<Collection>) -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream);
    let config = match collection {
        Some(settings) => config.collection(settings),
        None => config,
    };
    Lab::start(config).map(Some)
}

fn set_acl(lab: &Lab, path: &str, perm: Option<Perm>) -> Result<()> {
    let request = Request::SetAcl {
        space: SPACE.into(),
        path: SpacePath::try_from(path)?,
        principal: "user:bob".into(),
        perm,
    };
    lab.rpc(ADMIN, &request)?;
    Ok(())
}

/// On b, opens `shared/f` at v1 and reads its first page; a commits v2; after T_vis, b reads the
/// last page through the same handle. With `late_grant`, bob's only access is read on `shared`,
/// granted after v1; otherwise he keeps his write on the root, which is older than v1. Returns
/// what that read gave: the page of `v1` or `v2`, `neither`, or its failure.
fn read_on_after_a_commit(late_grant: bool) -> Result<Option<String>> {
    let Some(lab) = start(None)? else {
        return Ok(None);
    };
    let (old, new) = (random_bytes(LEN), random_bytes(LEN));
    fs::create_dir(lab.mount_path("a").join("shared"))?;
    save(&lab.mount_path("a").join("shared/f"), &old)?;
    if late_grant {
        set_acl(&lab, "shared", Some(Perm::Read))?;
        set_acl(&lab, "", None)?;
    }
    thread::sleep(VISIBLE_AFTER);
    let file = File::open(lab.mount_path("b").join("shared/f"))?;
    assert_bytes(&read_at(&file, 0, PAGE)?, &old[..PAGE], "the first page, at v1");

    save(&lab.mount_path("a").join("shared/f"), &new)?;
    thread::sleep(VISIBLE_AFTER);
    let last = LEN - PAGE;
    Ok(Some(match read_at(&file, last as u64, PAGE) {
        Ok(page) if page == new[last..] => "v2".into(),
        Ok(page) if page == old[last..] => "v1".into(),
        Ok(_) => "neither".into(),
        Err(error) => format!("failed: {error}"),
    }))
}

/// Repro: [`read_on_after_a_commit`] with bob's access granted after v1. Once v2 is the head, v1
/// is hidden from bob (S8), but the handle still reads v1, since nothing but a stat or an open
/// moves it on: the Grant for v1's last chunk answers NotFound and the read fails (EIO), where a
/// local file that another program rewrote reads on. The control is
/// [`a_reader_granted_before_v1_reads_on_after_a_commit`], whose grant is older than v1.
// contract: C3, S8 (bob may read the entry's current head)
#[test]
#[ignore = "ROUND9: an open file's reads fail once a commit hides the version it follows from the reader (S8)"]
fn a_reader_granted_after_v1_reads_on_after_a_commit() -> Result<()> {
    let Some(read) = read_on_after_a_commit(true)? else {
        return Ok(());
    };
    assert!(["v1", "v2"].contains(&read.as_str()), "the last page: {read}");
    Ok(())
}

// contract: C3
#[test]
fn a_reader_granted_before_v1_reads_on_after_a_commit() -> Result<()> {
    let Some(read) = read_on_after_a_commit(false)? else {
        return Ok(());
    };
    assert!(["v1", "v2"].contains(&read.as_str()), "the last page: {read}");
    Ok(())
}

/// The ids of the versions of `path` in [`SPACE`], as the admin sees them.
fn history(lab: &Lab, path: &str) -> Result<Vec<VersionId>> {
    let request = Request::History {
        space: SPACE.into(),
        path: SpacePath::try_from(path)?,
    };
    match lab.rpc(ADMIN, &request)? {
        Response::History { versions } => Ok(versions.iter().map(|version| version.id).collect()),
        reply => bail!("history answers with versions, not {reply:?}"),
    }
}

/// Waits until the authority no longer keeps `version` of `path`.
fn wait_pruned(lab: &Lab, path: &str, version: VersionId) -> Result<()> {
    let deadline = Instant::now() + PRUNED_WITHIN;
    while history(lab, path)?.contains(&version) {
        if Instant::now() > deadline {
            bail!("version {} of {path} is kept after {PRUNED_WITHIN:?}", version.0);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// b opens `f` (v1, written by a) for writing without truncating it, as a program that patches a
/// file in place does; a commits three newer versions; with `collection`, the test waits until
/// the authority prunes v1. Then b writes a page into the middle of `f` and fsyncs. Returns what
/// a reads there after T_vis, `b's page` or `another page`, or the failure of b's write.
fn patch_after_three_commits(collection: Option<Collection>) -> Result<Option<String>> {
    let Some(lab) = start(collection)? else {
        return Ok(None);
    };
    save(&lab.mount_path("a").join("f"), &random_bytes(LEN))?;
    let v1 = lab.head("alice", "f")?.id;
    thread::sleep(VISIBLE_AFTER);
    let file = OpenOptions::new()
        .write(true)
        .open(lab.mount_path("b").join("f"))?;
    for _ in 0..3 {
        save(&lab.mount_path("a").join("f"), &random_bytes(LEN))?;
    }
    if collection.is_some() {
        wait_pruned(&lab, "f", v1)?;
    }
    let page = random_bytes(PAGE);
    if let Err(error) = file.write_all_at(&page, MIDDLE).and_then(|()| file.sync_all()) {
        return Ok(Some(format!("failed: {error}")));
    }
    thread::sleep(VISIBLE_AFTER);
    let on_a = read_at(&File::open(lab.mount_path("a").join("f"))?, MIDDLE, PAGE)?;
    Ok(Some(
        if on_a == page { "b's page" } else { "another page" }.into(),
    ))
}

/// Repro: [`patch_after_three_commits`] with `Collection::QUICK`, which keeps 3 versions. The
/// handle still follows v1, which b's first write materializes as the base of its journal copy;
/// v1 is pruned, its Grant answers NotFound, and write(2) fails with ENOENT: b's save, the last
/// one, is lost. The control is [`a_writer_patches_a_file_whose_version_stays_retained`], under
/// the default retention.
// contract: C5, C14, C3
#[test]
#[ignore = "ROUND9: a write to a file held open fails with ENOENT once the version it follows leaves retention"]
fn a_writer_patches_a_file_whose_version_left_retention() -> Result<()> {
    let Some(patched) = patch_after_three_commits(Some(Collection::QUICK))? else {
        return Ok(());
    };
    assert_eq!(patched, "b's page", "the middle of f, on a");
    Ok(())
}

// contract: C5
#[test]
fn a_writer_patches_a_file_whose_version_stays_retained() -> Result<()> {
    let Some(patched) = patch_after_three_commits(None)? else {
        return Ok(());
    };
    assert_eq!(patched, "b's page", "the middle of f, on a");
    Ok(())
}

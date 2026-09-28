//! Round 21: a FUSE handle whose version is pruned and swept while it stays open.

use std::fs::File;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Request, Response, Version, VersionId};
use herdfs_lab::{ADMIN, Collection, Lab, LabConfig, Profile, SPACE, Strategy};

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, read_at, save};

const LEN: usize = 12 << 20;
const PAGE: usize = 4096;
const COLLECTED_WITHIN: Duration = Duration::from_secs(15);

fn start() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
        .collection(Collection::QUICK);
    Lab::start(config).map(Some)
}

fn history(lab: &Lab) -> Result<Vec<Version>> {
    let request = Request::History {
        space: SPACE.into(),
        path: SpacePath::try_from("f")?,
    };
    match lab.rpc(ADMIN, &request)? {
        Response::History { versions } => Ok(versions),
        reply => bail!("history answered {reply:?}"),
    }
}

fn wait_until(label: &str, mut done: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + COLLECTED_WITHIN;
    while !done()? {
        if Instant::now() > deadline {
            bail!("{label} after {COLLECTED_WITHIN:?}");
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Repro: b opens v1 and reads its start. After three newer saves, QUICK collection prunes v1
/// and deletes all of its unique chunks. Reading its uncached tail through the same descriptor
/// must move the handle to the retained head and return that committed version.
// contract: C3, C14
#[test]
fn an_open_reader_moves_on_after_its_version_is_collected() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let path_a = lab.mount_path("a").join("f");
    save(&path_a, &random_bytes(LEN))?;
    let first = lab.head(ADMIN, "f")?;
    thread::sleep(VISIBLE_AFTER);
    let held = File::open(lab.mount_path("b").join("f"))?;
    read_at(&held, 0, PAGE)?;

    let mut latest = Vec::new();
    for _ in 0..3 {
        latest = random_bytes(LEN);
        save(&path_a, &latest)?;
    }
    wait_until("v1 was not pruned", || {
        Ok(!history(&lab)?.iter().any(|version| version.id == first.id))
    })?;
    let first_keys: Vec<String> = first
        .chunks
        .iter()
        .map(|chunk| format!("chunks/{}", chunk.id))
        .collect();
    wait_until("v1 chunks were not swept", || {
        let stored = lab.stored("chunks/")?;
        Ok(first_keys.iter().all(|key| !stored.contains_key(key)))
    })?;

    let tail = LEN - PAGE;
    assert_bytes(
        &read_at(&held, tail as u64, PAGE)?,
        &latest[tail..],
        "the held file's tail after v1 was swept",
    );
    let head: VersionId = lab.head(ADMIN, "f")?.id;
    assert_eq!(history(&lab)?.last().map(|version| version.id), Some(head));
    Ok(())
}

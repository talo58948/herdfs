//! Erasure: what the authority deletes, prunes or purges leaves the store within grace plus
//! interval (S7, C15), what it keeps stays readable (C14), history before a grant stays hidden
//! (S8), and grant URLs die with their TTL (S4). The authority runs with `Collection::QUICK`, and
//! every check asks MinIO itself: its bucket listing, reads with its own credentials, and the
//! files in its data directory, which hold object bytes verbatim. Skips until the binaries are
//! built.

mod access;
mod delete;
mod retention;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use herdfs_core::proto::{self, EntryId, Request, Response, Version};
use herdfs_core::{ChunkRef, SpacePath, chunk};
use herdfs_lab::{Collection, Hit, Lab, LabConfig, Profile, SPACE, Strategy, sweep};
use reqwest::StatusCode;

/// Client and user names are the same.
const ALICE: &str = "alice";
const BOB: &str = "bob";
const QUICK: Collection = Collection::QUICK;
/// S7's bound, grace plus interval, and a margin for the collection pass and MinIO's cleanup.
const ERASED_WITHIN: Duration = Duration::from_secs(QUICK.grace.as_secs() + QUICK.every.as_secs() + 3);
/// Longer than a chunk can be, so a secret always has several.
const SECRET_LEN: usize = chunk::MAX_CHUNK + (1 << 20);

fn start() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client(ALICE, ALICE, &[], Profile::Lan, Strategy::Stream)
        .client(BOB, BOB, &[], Profile::Lan, Strategy::Stream)
        .collection(QUICK);
    Lab::start(config).map(Some)
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    rand::fill(&mut bytes[..]);
    bytes
}

/// Random bytes with `canary` at the start of every KiB and at the end, so every chunk of them
/// holds it.
fn secret(canary: &str, len: usize) -> Vec<u8> {
    let mut bytes = random_bytes(len);
    for run in bytes.chunks_mut(1024) {
        run[..canary.len()].copy_from_slice(canary.as_bytes());
    }
    bytes[len - canary.len()..].copy_from_slice(canary.as_bytes());
    bytes
}

/// Writes `bytes` to `path` in [`SPACE`] through alice's mount and returns the version committed.
fn write(lab: &Lab, path: &str, bytes: &[u8]) -> Result<Version> {
    let mut file = File::create(lab.mount_path(ALICE).join(path))?;
    file.write_all(bytes)?;
    // fsync returns once the authority has committed the version.
    file.sync_all()?;
    drop(file);
    lab.head(ALICE, path)
}

/// The bucket keys of `chunks`.
fn keys(chunks: &[ChunkRef]) -> BTreeSet<String> {
    chunks
        .iter()
        .map(|chunk| format!("chunks/{}", chunk.id))
        .collect()
}

/// The chunk objects in the bucket, by key, with their sizes.
fn stored(lab: &Lab) -> Result<BTreeMap<String, u64>> {
    lab.stored("chunks/")
}

/// The files of MinIO's that hold `canary`, except in its trash: MinIO moves a deleted object
/// there and empties it on its own schedule, which took about 5 minutes on a Linux test machine.
fn on_disk(lab: &Lab, canary: &str) -> Result<Vec<Hit>> {
    let trash = lab.store_dir().join(".minio.sys/tmp/.trash");
    let mut hits = sweep(&[lab.store_dir()], &[canary.to_owned()])?;
    hits.retain(|hit| !hit.path.starts_with(&trash));
    Ok(hits)
}

/// The status of a GET of `key` signed with the store's own credentials.
fn fetch(lab: &Lab, key: &str) -> Result<StatusCode> {
    Ok(lab.http(ALICE)?.get(lab.presign_get(key)).send()?.status())
}

/// The oracles see what they look for: `chunks` are listed and served by the bucket, and
/// `canary` is on disk.
fn assert_stored(lab: &Lab, chunks: &[ChunkRef], canary: &str) -> Result<()> {
    let stored = stored(lab)?;
    for key in keys(chunks) {
        assert!(stored.contains_key(&key), "{key} is not listed");
        assert_eq!(fetch(lab, &key)?, StatusCode::OK, "{key}");
    }
    assert!(
        !on_disk(lab, canary)?.is_empty(),
        "{canary} is on no file of MinIO's"
    );
    Ok(())
}

/// Polls `done` until it holds, failing after `timeout`.
fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !done()? {
        if Instant::now() > deadline {
            bail!("{what}: not after {timeout:?}");
        }
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// What of `chunks` the store still has: keys it lists, keys it serves, and files of its that
/// hold `canary`.
fn left(lab: &Lab, chunks: &[ChunkRef], canary: &str) -> Result<(Vec<String>, Vec<String>, Vec<Hit>)> {
    let (keys, stored) = (keys(chunks), stored(lab)?);
    let listed = keys
        .iter()
        .filter(|key| stored.contains_key(*key))
        .cloned()
        .collect();
    let mut served = Vec::new();
    for key in keys {
        if fetch(lab, &key)? != StatusCode::NOT_FOUND {
            served.push(key);
        }
    }
    Ok((listed, served, on_disk(lab, canary)?))
}

/// Waits until the store neither lists nor serves any of `chunks` and holds `canary` in no file
/// (S7), failing after `ERASED_WITHIN` with what is left.
fn wait_erased(lab: &Lab, chunks: &[ChunkRef], canary: &str) -> Result<()> {
    let result = wait_until("the bytes are erased", ERASED_WITHIN, || {
        let (listed, served, on_disk) = left(lab, chunks, canary)?;
        Ok(listed.is_empty() && served.is_empty() && on_disk.is_empty())
    });
    result.map_err(|error| match left(lab, chunks, canary) {
        Ok(left) => error.context(format!("listed, served, on disk: {left:?}")),
        Err(other) => other,
    })
}

/// The head of `path` in `space`, as `user` opens it.
fn head_in(lab: &Lab, user: &str, space: &str, path: &str) -> Result<Version> {
    let open = Request::Open {
        space: space.into(),
        path: SpacePath::try_from(path)?,
    };
    match lab.rpc(user, &open)? {
        Response::Opened {
            version: Some(version),
            ..
        } => Ok(version),
        reply => bail!("{space}:{path} has no head: {reply:?}"),
    }
}

/// `version`, granted to `user` in `space` and downloaded from the store, past any client's
/// cache.
fn read_version(lab: &Lab, user: &str, space: &str, version: &Version) -> Result<Vec<u8>> {
    let grant = Request::Grant {
        space: space.into(),
        entry: version.entry,
        version: version.id,
        chunks: (0..version.chunks.len() as u32).collect(),
    };
    let Response::Grants { urls, .. } = lab.rpc(user, &grant)? else {
        bail!("grant answers with URLs");
    };
    let http = lab.http(ALICE)?;
    let mut bytes = Vec::new();
    for url in urls {
        bytes.extend_from_slice(&http.get(url).send()?.error_for_status()?.bytes()?);
    }
    Ok(bytes)
}

/// Uploads the chunks of `bytes` to `space` as `user`, as a client's committer does.
fn upload(lab: &Lab, user: &str, space: &str, bytes: &[u8]) -> Result<Vec<ChunkRef>> {
    let (http, token) = (reqwest::blocking::Client::new(), lab.token(user, &[]));
    let mut chunks = Vec::new();
    for chunk in chunk::split(bytes) {
        let (chunk, bytes) = chunk?;
        let url = format!(
            "{}/v1/spaces/{space}/commits/erasure/chunks/{}",
            lab.authority_url(ALICE),
            chunk.id
        );
        http.put(url)
            .bearer_auth(&token)
            .body(bytes)
            .send()?
            .error_for_status()?;
        chunks.push(chunk);
    }
    Ok(chunks)
}

/// Creates an empty file at `path` in `space` as `user`.
fn create(lab: &Lab, user: &str, space: &str, path: &str) -> Result<EntryId> {
    let path = SpacePath::try_from(path)?;
    let parent_path = path.parent().unwrap_or_default();
    let Response::Entry { entry: parent } = lab.rpc(
        user,
        &Request::Stat {
            space: space.into(),
            path: parent_path,
        },
    )?
    else {
        bail!("stat answers with the entry")
    };
    let create = Request::Create {
        space: space.into(),
        parent: parent.id,
        name: path.name().into(),
        mode: 0o644,
    };
    match lab.rpc(user, &create)? {
        Response::Entry { entry } => Ok(entry.id),
        reply => bail!("create answers with the entry, not {reply:?}"),
    }
}

/// Commits `chunks` as the new head of `entry` in `space`, as `user`: the version, or the
/// authority's refusal.
fn commit(
    lab: &Lab,
    user: &str,
    space: &str,
    entry: EntryId,
    chunks: Vec<ChunkRef>,
) -> Result<Result<Version, proto::Error>> {
    let commit = Request::Commit {
        space: space.into(),
        entry,
        request_id: format!("{:032x}", rand::random::<u128>()),
        base: None,
        guarded: false,
        chunks,
        mtime_ms: None,
    };
    match lab.rpc(user, &commit) {
        Ok(Response::Version { version }) => Ok(Ok(version)),
        Ok(reply) => bail!("commit answers with a version, not {reply:?}"),
        Err(refusal) => Ok(Err(refusal)),
    }
}

/// Writes `bytes` to a new file at `path` in `space` through the protocol alone, as alice.
fn write_direct(lab: &Lab, space: &str, path: &str, bytes: &[u8]) -> Result<Version> {
    let chunks = upload(lab, ALICE, space, bytes)?;
    let entry = create(lab, ALICE, space, path)?;
    Ok(commit(lab, ALICE, space, entry, chunks)??)
}

/// The versions of `path` in [`SPACE`] that `user` may see, oldest first.
fn history(lab: &Lab, user: &str, path: &str) -> Result<Vec<Version>> {
    let request = Request::History {
        space: SPACE.into(),
        path: SpacePath::try_from(path)?,
    };
    match lab.rpc(user, &request)? {
        Response::History { versions } => Ok(versions),
        reply => bail!("history answers with versions, not {reply:?}"),
    }
}

//! What the scenarios share: starting a lab, saving and waiting, and calls to the authority.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request, Response, Version};
use herdfs_lab::{ADMIN, Lab, LabConfig, SPACE};

/// T_vis plus a margin for the test's own scheduling.
pub const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);
const POLL: Duration = Duration::from_millis(50);

/// Starts the lab, or skips the test with `None` until the binaries are built.
pub fn start(config: LabConfig) -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    Lab::start(config).map(Some)
}

pub fn path(text: &str) -> Result<SpacePath> {
    Ok(SpacePath::try_from(text)?)
}

/// Writes a file and returns once the authority has committed it: fsync waits for the commit.
pub fn save(file: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(file)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// The errno a file operation failed with, if it failed.
pub fn errno<T>(result: io::Result<T>) -> Option<i32> {
    result.err()?.raw_os_error()
}

/// `len` random bytes.
pub fn noise(len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    rand::fill(&mut bytes[..]);
    bytes
}

/// Polls `done` until it holds and returns how long that took, or fails after `timeout`.
pub fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> Result<bool>) -> Result<Duration> {
    let start = Instant::now();
    while !done()? {
        ensure!(start.elapsed() < timeout, "{what}: not after {timeout:?}");
        thread::sleep(POLL);
    }
    Ok(start.elapsed())
}

/// One of client `name`'s counters, such as `pending_commits`.
pub fn counter(lab: &Lab, name: &str, counter: &str) -> Result<u64> {
    let value = lab.stats(name)?[counter].as_u64();
    value.with_context(|| format!("the stats of {name} lack {counter}"))
}

/// Sets `principal`'s access to `at` in [`SPACE`] as the admin; `None` removes it.
pub fn set_acl(lab: &Lab, at: &str, principal: &str, perm: Option<Perm>) -> Result<()> {
    let acl = Request::SetAcl {
        space: SPACE.into(),
        path: path(at)?,
        principal: principal.into(),
        perm,
    };
    lab.rpc(ADMIN, &acl)?;
    Ok(())
}

/// Makes directory `at` in [`SPACE`] as the admin.
pub fn mkdir(lab: &Lab, at: &str) -> Result<()> {
    let (space, path, mode) = (SPACE.into(), path(at)?, 0o755);
    lab.rpc(ADMIN, &Request::Mkdir { space, path, mode })?;
    Ok(())
}

/// Every version of the file at `at` in [`SPACE`], oldest first, as `user` sees them.
pub fn history(lab: &Lab, user: &str, at: &str) -> Result<Vec<Version>> {
    let (space, path) = (SPACE.into(), path(at)?);
    match lab.rpc(user, &Request::History { space, path })? {
        Response::History { versions } => Ok(versions),
        other => bail!("History answered {other:?}"),
    }
}

/// The bytes of `version` of a file in [`SPACE`]: granted to `user`, then fetched over client
/// `name`'s store link as the client would.
pub fn version_bytes(lab: &Lab, name: &str, user: &str, version: &Version) -> Result<Vec<u8>> {
    let chunks = (0..version.chunks.len())
        .map(u32::try_from)
        .collect::<Result<_, _>>()?;
    let grant = Request::Grant {
        space: SPACE.into(),
        entry: version.entry,
        version: version.id,
        chunks,
    };
    let Response::Grants { urls, .. } = lab.rpc(user, &grant)? else {
        bail!("Grant answered something else");
    };
    let http = lab.http(name)?;
    let mut bytes = Vec::new();
    for url in urls {
        bytes.extend_from_slice(&http.get(url).send()?.error_for_status()?.bytes()?);
    }
    Ok(bytes)
}

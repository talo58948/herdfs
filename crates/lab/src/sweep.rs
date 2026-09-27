//! Canaries and sweeps: the oracle for bytes that leak where they must not.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

/// A unique marker to plant in secret files.
pub fn canary() -> String {
    format!("canary-{:032x}", rand::random::<u128>())
}

/// A canary found in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    pub canary: String,
}

/// Every file under `paths` that contains any of `canaries`, once per canary. Symlinks are
/// not followed, and files that vanish during the sweep are skipped.
pub fn sweep(paths: &[&Path], canaries: &[String]) -> io::Result<Vec<Hit>> {
    let mut hits = Vec::new();
    for path in paths {
        visit(path, canaries, &mut hits)?;
    }
    Ok(hits)
}

fn visit(path: &Path, canaries: &[String], hits: &mut Vec<Hit>) -> io::Result<()> {
    let Some(metadata) = unless_vanished(fs::symlink_metadata(path))? else {
        return Ok(());
    };
    if metadata.is_dir() {
        let Some(entries) = unless_vanished(fs::read_dir(path))? else {
            return Ok(());
        };
        for entry in entries {
            visit(&entry?.path(), canaries, hits)?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Ok(());
    }
    let Some(bytes) = unless_vanished(fs::read(path))? else {
        return Ok(());
    };
    let found = canaries
        .iter()
        .filter(|canary| contains(&bytes, canary.as_bytes()));
    hits.extend(found.map(|canary| Hit {
        path: path.to_owned(),
        canary: canary.clone(),
    }));
    Ok(())
}

fn unless_vanished<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        other => other.map(Some),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use herdfs_core::proto::Version;
use herdfs_core::{ChunkId, SpacePath};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Saved {
    pub roots: BTreeSet<SpacePath>,
    pub files: BTreeMap<SpacePath, SavedFile>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SavedFile {
    pub current: Version,
    #[serde(default)]
    pub retired: Vec<Version>,
    pub ready: bool,
}

pub(crate) struct Loaded {
    pub(super) saved: Saved,
    pub pinned: Vec<ChunkId>,
}

#[derive(Clone, Default)]
pub(super) struct Progress {
    pub downloading: bool,
    pub error: Option<String>,
}

pub(super) fn load(path: &Path) -> std::io::Result<Loaded> {
    let saved = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Saved::default(),
        Err(error) => return Err(error),
    };
    let pinned = saved
        .files
        .values()
        .flat_map(|file| std::iter::once(&file.current).chain(&file.retired))
        .flat_map(|version| version.chunks.iter().map(|chunk| chunk.id))
        .collect();
    Ok(Loaded { saved, pinned })
}

pub(super) fn persist(path: &Path, saved: &Saved) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("pin state has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temp, saved)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

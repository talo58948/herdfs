use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use herdfs_core::proto::{Entry, EntryId, Version};
use herdfs_core::{ChunkId, SpacePath};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Saved {
    #[serde(default, deserialize_with = "roots")]
    pub roots: BTreeMap<SpacePath, Option<EntryId>>,
    pub files: BTreeMap<SpacePath, SavedFile>,
    #[serde(default)]
    pub directories: BTreeMap<SpacePath, SavedDirectory>,
}

fn roots<'de, D>(deserializer: D) -> Result<BTreeMap<SpacePath, Option<EntryId>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Roots {
        Current(BTreeMap<SpacePath, Option<EntryId>>),
        Legacy(BTreeSet<SpacePath>),
    }
    Ok(match Roots::deserialize(deserializer)? {
        Roots::Current(roots) => roots,
        Roots::Legacy(roots) => roots.into_iter().map(|path| (path, None)).collect(),
    })
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct SavedDirectory {
    pub entry: EntryId,
    pub children: Vec<Entry>,
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

pub(crate) fn load(path: &Path) -> std::io::Result<Loaded> {
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
    fs::File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    // contract: P4, C7
    #[test]
    fn path_only_pins_migrate_and_entry_ids_round_trip() {
        let mut saved: Saved = serde_json::from_str(r#"{"roots":["kept"],"files":{}}"#).unwrap();
        let path = SpacePath::try_from("kept").unwrap();
        assert_eq!(saved.roots.get(&path), Some(&None));

        saved.roots.insert(path.clone(), Some(EntryId(7)));
        let encoded = serde_json::to_vec(&saved).unwrap();
        let restored: Saved = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored.roots.get(&path), Some(&Some(EntryId(7))));
    }
}

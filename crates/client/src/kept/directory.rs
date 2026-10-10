use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId, Kind, Version};

use super::{Kept, SavedFile, state::SavedDirectory, unpin_file};
use crate::fs::Space;
use crate::{Result, lock};

impl Kept {
    pub(super) fn ready(&self, space: &Space, root: &SpacePath) -> bool {
        let saved = lock(&self.saved);
        saved.roots.contains_key(root)
            && !lock(&self.pending).contains(root)
            && !lock(&self.downloads)
                .get(root)
                .is_some_and(|download| download.pending)
            && !lock(&self.errors).contains_key(root)
            && saved
                .files
                .iter()
                .filter(|(path, _)| path.is_within(root))
                .all(|(path, file)| {
                    file.ready
                        && space
                            .inodes
                            .at(path)
                            .is_none_or(|(entry, _, _)| entry.head == Some(file.current.id))
                })
    }

    pub(crate) fn listing(&self, space: &Space, entry: EntryId, path: &SpacePath) -> Option<Vec<Entry>> {
        let saved = lock(&self.saved);
        let directory = saved.directories.get(path)?;
        if directory.entry != entry {
            return None;
        }
        let entries = directory.children.clone();
        let roots: Vec<_> = saved
            .roots
            .keys()
            .filter(|root| path.is_within(root))
            .cloned()
            .collect();
        drop(saved);
        roots
            .iter()
            .any(|root| self.ready(space, root))
            .then_some(entries)
    }

    pub(super) fn finish_scan(
        &self,
        space: &Space,
        root: &SpacePath,
        seen: &BTreeSet<SpacePath>,
        directories: BTreeMap<SpacePath, SavedDirectory>,
    ) -> std::io::Result<()> {
        let removed = self
            .update(|saved| {
                let paths: Vec<_> = saved
                    .files
                    .keys()
                    .filter(|path| path.is_within(root) && !seen.contains(*path))
                    .cloned()
                    .collect();
                let unchanged = saved
                    .directories
                    .iter()
                    .filter(|(path, _)| path.is_within(root))
                    .eq(directories.iter());
                if paths.is_empty() && unchanged {
                    return None;
                }
                let removed = paths
                    .into_iter()
                    .filter_map(|path| saved.files.remove(&path))
                    .collect::<Vec<SavedFile>>();
                saved.directories.retain(|path, _| !path.is_within(root));
                saved.directories.extend(directories);
                Some(removed)
            })?
            .unwrap_or_default();
        removed.iter().for_each(|file| unpin_file(space, file));
        Ok(())
    }

    pub(super) async fn scan(
        &self,
        space: &Space,
        entry: Entry,
    ) -> Result<(
        Vec<(SpacePath, Entry, Option<Version>)>,
        Vec<EntryId>,
        BTreeMap<SpacePath, SavedDirectory>,
    )> {
        let asked = Instant::now();
        space.inodes.observe(&entry, asked, false);
        let mut stack = vec![entry];
        let mut files = Vec::new();
        let mut entries = Vec::new();
        let mut directories = BTreeMap::new();
        while let Some(entry) = stack.pop() {
            entries.push(entry.id);
            match entry.kind {
                Kind::Dir => {
                    let asked = Instant::now();
                    let children = space.remote.list(entry.id).await?;
                    children
                        .iter()
                        .for_each(|child| space.inodes.observe(child, asked, true));
                    directories.insert(
                        entry.path,
                        SavedDirectory {
                            entry: entry.id,
                            children: children.clone(),
                        },
                    );
                    stack.extend(children);
                }
                Kind::File => {
                    let asked = Instant::now();
                    let (opened, version, _) = space.remote.open(entry.id).await?;
                    space.inodes.observe(&opened, asked, false);
                    files.push((opened.path.clone(), opened, version));
                }
                Kind::Symlink => {}
            }
        }
        Ok((files, entries, directories))
    }
}

use std::collections::{BTreeSet, HashSet};
use std::time::Instant;

use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId, Kind, Version};

use super::Kept;
use crate::Result;
use crate::lock;
use crate::version::OpenVersion;

impl Kept {
    pub(super) async fn refresh(&self) {
        let roots: Vec<_> = lock(&self.saved).roots.iter().cloned().collect();
        let mut seen = BTreeSet::new();
        let mut entries = HashSet::new();
        let mut complete = true;
        for root in roots {
            match self.scan(&root).await {
                Ok((files, found)) => {
                    entries.extend(found);
                    for (path, entry, version) in files {
                        seen.insert(path.clone());
                        if let Err(error) = self.fetch(path, entry, version).await {
                            complete = false;
                            self.set_progress(&root, false, Some(error.to_string()));
                        }
                    }
                    if lock(&self.progress)
                        .get(&root)
                        .is_some_and(|state| state.error.is_none())
                    {
                        self.set_progress(&root, false, None);
                    }
                }
                Err(error) => {
                    complete = false;
                    self.set_progress(&root, false, Some(error.to_string()));
                }
            }
        }
        if complete {
            self.drop_unseen(&seen);
            self.inodes.keep(entries);
        }
    }

    async fn fetch(&self, path: SpacePath, entry: Entry, version: Option<Version>) -> Result<()> {
        self.inodes.observe(&entry, Instant::now(), false);
        let Some(version) = version else {
            return Ok(());
        };
        let downloading = self.prepare(&path, &version)?;
        if downloading {
            self.set_path_progress(&path, true);
        }
        let open = OpenVersion::new(version.clone());
        self.reader.cache_all(&open).await?;
        if !version
            .chunks
            .iter()
            .all(|chunk| self.cache.get(&chunk.id).is_some())
        {
            return Err(crate::Error::Corrupt(version.chunks[0].id));
        }
        self.finish(&path)?;
        let switched = self
            .inodes
            .with(entry.id, |state| state.switch((Some(version), true)));
        if downloading || switched == Some(true) {
            self.kernel.invalidate_attrs(entry.id);
        }
        Ok(())
    }

    pub(super) async fn scan(
        &self,
        root: &SpacePath,
    ) -> Result<(Vec<(SpacePath, Entry, Option<Version>)>, Vec<EntryId>)> {
        let entry = self.remote.stat(root.clone()).await?;
        let mut stack = vec![entry];
        let mut files = Vec::new();
        let mut entries = Vec::new();
        while let Some(entry) = stack.pop() {
            entries.push(entry.id);
            self.inodes.observe(&entry, Instant::now(), false);
            match entry.kind {
                Kind::Dir => stack.extend(self.remote.list(entry.id).await?),
                Kind::File => {
                    let (opened, version, _) = self.remote.open(entry.id).await?;
                    files.push((opened.path.clone(), opened, version));
                }
                Kind::Symlink => {}
            }
        }
        Ok((files, entries))
    }
}

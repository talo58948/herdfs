use std::collections::HashSet;
use std::time::Instant;

use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId, Kind, Version};

use super::Kept;
use crate::fs::Space;
use crate::version::OpenVersion;
use crate::{Error, Result, lock};

impl Kept {
    pub(super) async fn refresh(&self, space: &Space) {
        let roots: Vec<_> = lock(&self.saved).roots.iter().cloned().collect();
        let mut entries = HashSet::new();
        for root in roots {
            match self.scan(space, &root).await {
                Ok((files, found)) => {
                    let seen = files.iter().map(|(path, _, _)| path.clone()).collect();
                    let mut error = None;
                    for (path, entry, version) in files {
                        if let Err(failed) = self.fetch(space, path, entry, version).await {
                            error.get_or_insert_with(|| failed.to_string());
                        }
                    }
                    self.set_error(&root, error);
                    self.drop_unseen(space, &root, &seen);
                    entries.extend(found);
                }
                Err(error) => {
                    entries.extend(space.inodes.within(&root));
                    if !error.is_offline() {
                        self.set_error(&root, Some(error.to_string()));
                    }
                }
            }
        }
        space.inodes.keep(entries);
    }

    async fn fetch(
        &self,
        space: &Space,
        path: SpacePath,
        entry: Entry,
        version: Option<Version>,
    ) -> Result<()> {
        let Some(version) = version else {
            return Ok(());
        };
        let Some(downloading) = self.prepare(space, &path, &version)? else {
            return Ok(());
        };
        if downloading {
            space.reader.cache_all(&OpenVersion::new(version.clone())).await?;
        }
        if let Some(chunk) = version
            .chunks
            .iter()
            .find(|chunk| !space.cache.contains(&chunk.id))
        {
            return Err(Error::Corrupt(chunk.id));
        }
        self.finish(space, &path)?;
        let switched = space
            .inodes
            .with(entry.id, |state| state.switch((Some(version), true)));
        if downloading || switched == Some(true) {
            space.kernel.invalidate_attrs(entry.id);
        }
        Ok(())
    }

    async fn scan(
        &self,
        space: &Space,
        root: &SpacePath,
    ) -> Result<(Vec<(SpacePath, Entry, Option<Version>)>, Vec<EntryId>)> {
        let asked = Instant::now();
        let entry = space.remote.stat(root.clone()).await?;
        space.inodes.observe(&entry, asked, false);
        let mut stack = vec![entry];
        let mut files = Vec::new();
        let mut entries = Vec::new();
        while let Some(entry) = stack.pop() {
            entries.push(entry.id);
            match entry.kind {
                Kind::Dir => {
                    let asked = Instant::now();
                    let children = space.remote.list(entry.id).await?;
                    children
                        .iter()
                        .for_each(|child| space.inodes.observe(child, asked, true));
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
        Ok((files, entries))
    }
}

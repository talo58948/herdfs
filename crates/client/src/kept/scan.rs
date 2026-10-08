use std::time::Instant;

use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId, Kind, Version};

use super::Kept;
use crate::Result;

impl Kept {
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

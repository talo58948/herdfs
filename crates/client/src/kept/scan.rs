use std::collections::{HashMap, HashSet};
use std::time::Instant;

use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId, Kind, Version};

use super::{Kept, unpin_file};
use crate::fs::Space;
use crate::version::OpenVersion;
use crate::{Error, Result, lock};

impl Kept {
    pub(super) async fn heads_changed(&self, space: &Space) -> bool {
        let saved = lock(&self.saved).clone();
        for root in saved.roots.keys() {
            if let Some(entry) = saved.roots[root] {
                let current = match space.remote.locate(entry).await {
                    Ok(current) => current,
                    Err(error) if error.is_offline() => return false,
                    Err(_) => {
                        lock(&self.pending).insert(root.clone());
                        return true;
                    }
                };
                let version = saved.files.get(root).map(|file| file.current.id);
                if current.path != *root || current.kind == Kind::File && current.head != version {
                    lock(&self.pending).insert(root.clone());
                    return true;
                }
            }
            let entries: Vec<_> = space
                .inodes
                .within(root)
                .into_iter()
                .filter_map(|id| space.inodes.with(id, |state| state.cached()).flatten())
                .map(|(entry, _, _)| entry)
                .collect();
            let mut known: HashMap<_, HashSet<_>> = HashMap::new();
            for entry in &entries {
                if let Some(parent) = entry.path.parent() {
                    known
                        .entry(parent)
                        .or_default()
                        .insert((entry.id, entry.path.clone()));
                }
            }
            for directory in entries.iter().filter(|entry| entry.kind == Kind::Dir) {
                let listed = match space.remote.list(directory.id).await {
                    Ok(listed) => listed,
                    Err(error) if error.is_offline() => return false,
                    Err(_) => {
                        lock(&self.pending).insert(root.clone());
                        return true;
                    }
                };
                let listed_entries: HashSet<_> = listed
                    .iter()
                    .map(|entry| (entry.id, entry.path.clone()))
                    .collect();
                let heads_changed = listed.iter().any(|entry| {
                    entry.kind == Kind::File
                        && entry.head != saved.files.get(&entry.path).map(|file| file.current.id)
                });
                if known.remove(&directory.path).unwrap_or_default() != listed_entries || heads_changed {
                    lock(&self.pending).insert(root.clone());
                    return true;
                }
            }
        }
        false
    }

    pub(super) async fn refresh(&self, space: &Space) {
        let roots: Vec<_> = lock(&self.saved)
            .roots
            .iter()
            .map(|(path, entry)| (path.clone(), *entry))
            .collect();
        let mut entries = HashSet::new();
        for (saved_root, saved_entry) in roots {
            let located = match saved_entry {
                Some(entry) => space.remote.locate(entry).await,
                None => space.remote.stat(saved_root.clone()).await,
            };
            let entry = match located {
                Ok(entry) => entry,
                Err(error) if error.is_not_found() => {
                    if lock(&self.saved).roots.get(&saved_root).copied() != Some(saved_entry) {
                        continue;
                    }
                    if let Err(error) = self.unpin(space, &saved_root) {
                        tracing::warn!(%error, root = %saved_root, "could not drop a deleted kept root");
                    }
                    continue;
                }
                Err(error) => {
                    entries.extend(space.inodes.within(&saved_root));
                    if !error.is_offline() {
                        self.set_error(&saved_root, Some(error.to_string()));
                    }
                    continue;
                }
            };
            let root = entry.path.clone();
            if let Err(error) = self.relocate(space, &saved_root, &root, entry.id) {
                self.set_error(&saved_root, Some(error.to_string()));
                entries.extend(space.inodes.within(&saved_root));
                continue;
            }
            match self.scan(space, entry).await {
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
                    lock(&self.pending).remove(&root);
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

    pub(super) fn relocate(
        &self,
        space: &Space,
        from: &SpacePath,
        to: &SpacePath,
        entry: EntryId,
    ) -> std::io::Result<()> {
        let Some(replaced) = self.update(|saved| {
            let known = saved.roots.get(from).copied()?;
            if known.is_some_and(|known| known != entry) || from == to && known == Some(entry) {
                return None;
            }
            saved.roots.remove(from);
            saved.roots.insert(to.clone(), Some(entry));
            let paths: Vec<_> = saved
                .files
                .keys()
                .filter(|path| path.is_within(from))
                .cloned()
                .collect();
            let mut replaced = Vec::new();
            for path in paths {
                let Some(rebased) = path.rebase(from, to) else {
                    continue;
                };
                let Some(file) = saved.files.remove(&path) else {
                    continue;
                };
                replaced.extend(saved.files.insert(rebased, file));
            }
            Some(replaced)
        })?
        else {
            return Ok(());
        };
        if lock(&self.pending).remove(from) {
            lock(&self.pending).insert(to.clone());
        }
        if let Some(error) = lock(&self.errors).remove(from) {
            lock(&self.errors).insert(to.clone(), error);
        }
        replaced.iter().for_each(|file| unpin_file(space, file));
        Ok(())
    }

    async fn fetch(
        &self,
        space: &Space,
        path: SpacePath,
        entry: Entry,
        version: Option<Version>,
    ) -> Result<()> {
        let Some(version) = version else {
            let switched = space.inodes.with(entry.id, |state| state.switch((None, true)));
            if switched == Some(true) {
                space.kernel.invalidate_attrs(entry.id);
            }
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
        entry: Entry,
    ) -> Result<(Vec<(SpacePath, Entry, Option<Version>)>, Vec<EntryId>)> {
        let asked = Instant::now();
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

//! Durable keep-downloaded roots and the latest cached versions below them (P4).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use herdfs_core::SpacePath;
use herdfs_core::proto::Version;
use tokio::sync::Notify;

use crate::fs::Space;
use crate::{Result, lock};

const REFRESH: Duration = Duration::from_secs(60);

mod scan;
mod state;
pub(crate) use state::load;
use state::{Loaded, Saved, SavedFile};

pub(crate) struct Kept {
    path: PathBuf,
    saved: Mutex<Saved>,
    pending: Mutex<BTreeSet<SpacePath>>,
    errors: Mutex<BTreeMap<SpacePath, String>>,
    write: Mutex<()>,
    wake: Notify,
}

impl Kept {
    pub fn new(path: PathBuf, loaded: Loaded) -> Self {
        let pending = loaded.saved.roots.keys().cloned().collect();
        Self {
            path,
            saved: Mutex::new(loaded.saved),
            pending: Mutex::new(pending),
            errors: Mutex::default(),
            write: Mutex::default(),
            wake: Notify::new(),
        }
    }

    pub async fn run(self: Arc<Self>, space: Arc<Space>) {
        loop {
            self.refresh(&space).await;
            let _ = tokio::time::timeout(REFRESH, self.wake.notified()).await;
        }
    }

    pub async fn pin(&self, space: &Space, path: &SpacePath) -> Result<()> {
        if lock(&self.saved).roots.contains_key(path) {
            if lock(&self.errors).remove(path).is_some() {
                lock(&self.pending).insert(path.clone());
            }
            self.wake.notify_one();
            return Ok(());
        }
        let entry = space.remote.stat(path.clone()).await?;
        let added = self
            .update(|saved| {
                saved
                    .roots
                    .insert(path.clone(), Some(entry.id))
                    .is_none()
                    .then_some(())
            })?
            .is_some();
        let retry = lock(&self.errors).remove(path).is_some();
        if added || retry {
            lock(&self.pending).insert(path.clone());
        }
        self.wake.notify_one();
        Ok(())
    }

    pub fn unpin(&self, space: &Space, path: &SpacePath) -> std::io::Result<()> {
        let removed = self
            .update(|saved| {
                if saved.roots.remove(path).is_none() {
                    return None;
                }
                let roots: Vec<_> = saved.roots.keys().cloned().collect();
                let paths: Vec<_> = saved
                    .files
                    .keys()
                    .filter(|file| !roots.iter().any(|root| file.is_within(root)))
                    .cloned()
                    .collect();
                Some(
                    paths
                        .into_iter()
                        .filter_map(|file| saved.files.remove(&file))
                        .collect::<Vec<_>>(),
                )
            })?
            .unwrap_or_default();
        lock(&self.pending).remove(path);
        lock(&self.errors).remove(path);
        removed.iter().for_each(|file| unpin_file(space, file));
        self.wake.notify_one();
        Ok(())
    }

    pub fn status(&self, space: &Space) -> (u64, Vec<(String, bool)>) {
        let saved = lock(&self.saved);
        let pending = lock(&self.pending);
        let errors = lock(&self.errors);
        let mut ids = HashSet::new();
        let bytes = saved
            .files
            .values()
            .flat_map(|file| &file.current.chunks)
            .filter(|chunk| ids.insert(chunk.id) && space.cache.contains(&chunk.id))
            .map(|chunk| u64::from(chunk.len))
            .sum();
        let pins = saved
            .roots
            .keys()
            .map(|root| {
                let ready = !pending.contains(root)
                    && !errors.contains_key(root)
                    && saved
                        .files
                        .iter()
                        .filter(|(path, _)| path.is_within(root))
                        .all(|(_, file)| file.ready);
                (root.as_str().to_owned(), ready)
            })
            .collect();
        (bytes, pins)
    }

    pub fn file(&self, path: &SpacePath) -> (bool, bool, Option<String>) {
        let saved = lock(&self.saved);
        let roots: Vec<_> = saved
            .roots
            .keys()
            .filter(|root| path.is_within(root))
            .collect();
        let downloading = saved.files.get(path).is_some_and(|file| !file.ready);
        let errors = lock(&self.errors);
        let error = roots.iter().find_map(|root| errors.get(*root).cloned());
        (!roots.is_empty(), downloading, error)
    }

    fn prepare(&self, space: &Space, path: &SpacePath, version: &Version) -> std::io::Result<Option<bool>> {
        let cached = version.chunks.iter().all(|chunk| space.cache.contains(&chunk.id));
        let _write = lock(&self.write);
        let mut saved = lock(&self.saved).clone();
        if !saved.roots.keys().any(|root| path.is_within(root)) {
            return Ok(None);
        }
        let Some(file) = saved.files.get_mut(path) else {
            let file = SavedFile {
                current: version.clone(),
                retired: Vec::new(),
                ready: false,
            };
            saved.files.insert(path.clone(), file);
            state::persist(&self.path, &saved)?;
            space.cache.pin(&chunk_ids(version));
            *lock(&self.saved) = saved;
            return Ok(Some(true));
        };
        if file.current.id != version.id {
            let retired = std::iter::once(file.current.clone())
                .chain(std::mem::take(&mut file.retired))
                .collect();
            *file = SavedFile {
                current: version.clone(),
                retired,
                ready: false,
            };
            state::persist(&self.path, &saved)?;
            space.cache.pin(&chunk_ids(version));
            *lock(&self.saved) = saved;
            return Ok(Some(true));
        }
        if cached {
            return Ok(Some(false));
        }
        if file.ready {
            file.ready = false;
            state::persist(&self.path, &saved)?;
            *lock(&self.saved) = saved;
        }
        Ok(Some(true))
    }

    fn finish(&self, space: &Space, path: &SpacePath) -> std::io::Result<()> {
        let _write = lock(&self.write);
        let mut saved = lock(&self.saved).clone();
        let Some(file) = saved.files.get_mut(path) else {
            return Ok(());
        };
        if file.ready && file.retired.is_empty() {
            return Ok(());
        }
        file.ready = true;
        let retired = std::mem::take(&mut file.retired);
        state::persist(&self.path, &saved)?;
        *lock(&self.saved) = saved;
        retired.iter().for_each(|version| unpin_version(space, version));
        Ok(())
    }

    fn drop_unseen(&self, space: &Space, root: &SpacePath, seen: &BTreeSet<SpacePath>) {
        let removed = self.update(|saved| {
            let paths: Vec<_> = saved
                .files
                .keys()
                .filter(|path| path.is_within(root) && !seen.contains(*path))
                .cloned()
                .collect();
            (!paths.is_empty()).then(|| {
                paths
                    .into_iter()
                    .filter_map(|path| saved.files.remove(&path))
                    .collect::<Vec<_>>()
            })
        });
        match removed {
            Ok(Some(removed)) => removed.iter().for_each(|file| unpin_file(space, file)),
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, %root, "could not persist removed kept files"),
        }
    }

    fn set_error(&self, root: &SpacePath, error: Option<String>) {
        let mut errors = lock(&self.errors);
        match error {
            Some(error) => {
                errors.insert(root.clone(), error);
            }
            None => {
                errors.remove(root);
            }
        }
    }

    fn update<T>(&self, change: impl FnOnce(&mut Saved) -> Option<T>) -> std::io::Result<Option<T>> {
        let _write = lock(&self.write);
        let mut saved = lock(&self.saved).clone();
        let Some(changed) = change(&mut saved) else {
            return Ok(None);
        };
        state::persist(&self.path, &saved)?;
        *lock(&self.saved) = saved;
        Ok(Some(changed))
    }
}

fn chunk_ids(version: &Version) -> Vec<herdfs_core::ChunkId> {
    version.chunks.iter().map(|chunk| chunk.id).collect()
}

fn unpin_file(space: &Space, file: &SavedFile) {
    unpin_version(space, &file.current);
    file.retired
        .iter()
        .for_each(|version| unpin_version(space, version));
}

fn unpin_version(space: &Space, version: &Version) {
    space.cache.unpin(&chunk_ids(version));
}

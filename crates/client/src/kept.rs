//! Durable keep-downloaded roots and the latest cached versions below them (P4).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Version};
use tokio::sync::Notify;

use crate::cache::Cache;
use crate::fs::{Inodes, Kernel};
use crate::reader::Reader;
use crate::remote::Remote;
use crate::version::OpenVersion;
use crate::{Result, lock};

const REFRESH: Duration = Duration::from_secs(1);

mod state;
mod scan;
use state::{Loaded, Progress, Saved, SavedFile};

pub(crate) struct Kept {
    path: PathBuf,
    saved: Mutex<Saved>,
    progress: Mutex<BTreeMap<SpacePath, Progress>>,
    wake: Notify,
    remote: Arc<Remote>,
    reader: Arc<Reader>,
    cache: Arc<Cache>,
    inodes: Arc<Inodes>,
    kernel: Arc<Kernel>,
}

impl Kept {
    pub fn load(path: &Path) -> std::io::Result<Loaded> {
        state::load(path)
    }

    pub fn new(
        path: PathBuf,
        loaded: Loaded,
        remote: Arc<Remote>,
        reader: Arc<Reader>,
        cache: Arc<Cache>,
        inodes: Arc<Inodes>,
        kernel: Arc<Kernel>,
    ) -> Self {
        let progress = loaded
            .saved
            .roots
            .iter()
            .cloned()
            .map(|root| (root, Progress { downloading: true, error: None }))
            .collect();
        Self {
            path,
            saved: Mutex::new(loaded.saved),
            progress: Mutex::new(progress),
            wake: Notify::new(),
            remote,
            reader,
            cache,
            inodes,
            kernel,
        }
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            self.refresh().await;
            let _ = tokio::time::timeout(REFRESH, self.wake.notified()).await;
        }
    }

    pub fn pin(&self, path: SpacePath) -> std::io::Result<()> {
        lock(&self.saved).roots.insert(path.clone());
        lock(&self.progress).insert(path, Progress { downloading: true, error: None });
        self.persist()?;
        self.wake.notify_one();
        Ok(())
    }

    pub fn unpin(&self, path: &SpacePath) -> std::io::Result<()> {
        let removed = {
            let mut saved = lock(&self.saved);
            saved.roots.remove(path);
            let roots = saved.roots.clone();
            let removed: Vec<_> = saved
                .files
                .keys()
                .filter(|file| !roots.iter().any(|root| file.is_within(root)))
                .cloned()
                .collect();
            removed
                .iter()
                .filter_map(|file| saved.files.remove(file))
                .collect::<Vec<_>>()
        };
        lock(&self.progress).remove(path);
        for file in removed {
            self.unpin_file(&file);
        }
        self.persist()?;
        Ok(())
    }

    pub fn status(&self) -> (u64, Vec<(String, bool)>) {
        let saved = lock(&self.saved);
        let progress = lock(&self.progress);
        let mut ids = HashSet::new();
        let bytes = saved
            .files
            .values()
            .flat_map(|file| &file.current.chunks)
            .filter(|chunk| ids.insert(chunk.id) && self.cache.contains(&chunk.id))
            .map(|chunk| u64::from(chunk.len))
            .sum();
        let pins = saved
            .roots
            .iter()
            .map(|root| {
                let ready = progress
                    .get(root)
                    .is_some_and(|state| !state.downloading && state.error.is_none())
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
        let roots: Vec<_> = saved.roots.iter().filter(|root| path.is_within(root)).collect();
        let progress = lock(&self.progress);
        let downloading = roots
            .iter()
            .any(|root| progress.get(*root).is_some_and(|state| state.downloading));
        let error = roots.iter().find_map(|root| progress.get(*root)?.error.clone());
        (!roots.is_empty(), downloading, error)
    }

    async fn refresh(&self) {
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
                    if lock(&self.progress).get(&root).is_some_and(|state| state.error.is_none()) {
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
        if !version.chunks.iter().all(|chunk| self.cache.get(&chunk.id).is_some()) {
            return Err(crate::Error::Corrupt(version.chunks[0].id));
        }
        self.finish(&path)?;
        let switched = self.inodes.with(entry.id, |state| state.switch((Some(version), true)));
        if downloading || switched == Some(true) {
            self.kernel.invalidate_attrs(entry.id);
        }
        Ok(())
    }

    fn prepare(&self, path: &SpacePath, version: &Version) -> std::io::Result<bool> {
        let mut saved = lock(&self.saved);
        if saved.files.get(path).is_some_and(|file| file.current.id == version.id) {
            let cached = version
                .chunks
                .iter()
                .all(|chunk| self.cache.get(&chunk.id).is_some());
            if cached {
                return Ok(false);
            }
            if let Some(file) = saved.files.get_mut(path) {
                file.ready = false;
            }
            drop(saved);
            self.persist()?;
            return Ok(true);
        }
        self.cache.pin(&version.chunks.iter().map(|chunk| chunk.id).collect::<Vec<_>>());
        let retired = saved.files.remove(path).map_or_else(Vec::new, |file| {
            std::iter::once(file.current).chain(file.retired).collect()
        });
        saved.files.insert(
            path.clone(),
            SavedFile { current: version.clone(), retired, ready: false },
        );
        drop(saved);
        self.persist()?;
        Ok(true)
    }

    fn finish(&self, path: &SpacePath) -> std::io::Result<()> {
        let retired = {
            let mut saved = lock(&self.saved);
            let Some(file) = saved.files.get_mut(path) else { return Ok(()) };
            file.ready = true;
            std::mem::take(&mut file.retired)
        };
        retired.iter().for_each(|version| self.unpin_version(version));
        self.persist()
    }

    fn drop_unseen(&self, seen: &BTreeSet<SpacePath>) {
        let removed = {
            let mut saved = lock(&self.saved);
            let paths: Vec<_> = saved.files.keys().filter(|path| !seen.contains(*path)).cloned().collect();
            paths.into_iter().filter_map(|path| saved.files.remove(&path)).collect::<Vec<_>>()
        };
        removed.iter().for_each(|file| self.unpin_file(file));
        let _ = self.persist();
    }

    fn unpin_file(&self, file: &SavedFile) {
        self.unpin_version(&file.current);
        file.retired.iter().for_each(|version| self.unpin_version(version));
    }

    fn unpin_version(&self, version: &Version) {
        self.cache.unpin(&version.chunks.iter().map(|chunk| chunk.id).collect::<Vec<_>>());
    }

    fn set_progress(&self, root: &SpacePath, downloading: bool, error: Option<String>) {
        if let Some(state) = lock(&self.progress).get_mut(root) {
            *state = Progress { downloading, error };
        }
    }

    fn set_path_progress(&self, path: &SpacePath, downloading: bool) {
        let mut progress = lock(&self.progress);
        for (root, state) in &mut *progress {
            if path.is_within(root) {
                state.downloading = downloading;
                state.error = None;
            }
        }
    }

    fn persist(&self) -> std::io::Result<()> {
        state::persist(&self.path, &lock(&self.saved))
    }
}

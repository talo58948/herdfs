//! The client's view of the namespace: the path of each inode the kernel holds and the entries
//! fetched from the authority, and how both stay current (C1, C2, C7).

use std::time::Instant;
use std::{ffi::OsStr, fs::File, sync::Arc};

use fuser::{Errno, FileAttr, INodeNo};
use herdfs_core::SpacePath;
use herdfs_core::proto::Entry;

use super::{Asked, MAX_NAME, RECENT, Space, TTL};
use crate::{Error, Result, lock};

impl Space {
    /// The inode's path, which another client may have moved: once older than TTL, asked of
    /// the authority by entry id (C7). Offline, the path held.
    pub(super) async fn located(&self, ino: INodeNo) -> Result<SpacePath> {
        let known = lock(&self.paths).get(&ino).cloned();
        let (path, _, confirmed) = known.ok_or(Errno::ENOENT)?;
        if ino == INodeNo::ROOT || confirmed.is_some_and(|confirmed| confirmed.elapsed() < TTL) {
            return Ok(path);
        }
        match self.relocate(ino).await {
            Err(err) if err.is_offline() => Ok(path),
            located => Ok(located?.path),
        }
    }

    /// The entry of the inode, wherever it is now.
    pub(super) async fn relocate(&self, ino: INodeNo) -> Result<Entry> {
        let asked = self.asked();
        let entry = match self.remote.locate(self.entry_id(ino)).await {
            Err(err) => return Err(self.gone(&self.path(ino)?, err)),
            located => located?,
        };
        self.follow(&entry, asked).await;
        Ok(entry)
    }

    /// The path of `name` in the directory `parent`, wherever that is now.
    pub(super) async fn located_child(&self, parent: INodeNo, name: &OsStr) -> Result<SpacePath> {
        if name.len() > MAX_NAME {
            return Err(Errno::ENAMETOOLONG.into());
        }
        let (name, parent) = (name.to_str().ok_or(Errno::EINVAL)?, self.located(parent).await?);
        Ok(parent.join(name).map_err(|_| Errno::EINVAL)?)
    }

    /// The entry at `path`, when it was asked for, and its journal copy while it holds changes.
    /// The entry comes from the cache for TTL after that, so opens see commits (C2), else from
    /// the authority; both are read in one generation of settled commits, counted before either,
    /// so a reply never pairs a remote size with a journal state from after a commit changed
    /// it (C1).
    pub(super) async fn entry(&self, path: &SpacePath) -> Result<(Entry, Instant, Option<Arc<File>>)> {
        loop {
            let asked = self.asked();
            let found = match self.cached(path, asked.1) {
                // A listing caches entries it never followed.
                Some(cached) => {
                    self.move_on(&cached.0).await;
                    Ok(cached)
                }
                None => self.fetch(path, asked).await,
            };
            let (entry, fetched) = match found {
                Err(err) if err.is_offline() => return self.held(path).ok_or(err),
                found => found?,
            };
            if let Some(local) = self.journal.changes_as_of(entry.id, asked.1) {
                return Ok((entry, fetched, local));
            }
        }
    }

    /// The entry cached at `path` while it is current: within TTL, from generation `settled`, and
    /// no older than the authority's latest word on its inode, such as its path after a rename
    /// (C3, C7).
    fn cached(&self, path: &SpacePath, settled: u64) -> Option<(Entry, Instant)> {
        let (fetched, generation, entry) = lock(&self.recent).get(path)?;
        let confirmed = lock(&self.paths)
            .get(&self.ino(entry.id))
            .and_then(|known| known.2);
        let current = generation == settled && fetched.elapsed() < TTL && confirmed <= Some(fetched);
        current.then_some((entry, fetched))
    }

    async fn fetch(&self, path: &SpacePath, asked: Asked) -> Result<(Entry, Instant)> {
        let entry = self.remote.stat(path.clone()).await?;
        self.follow(&entry, asked).await;
        Ok((entry, asked.0))
    }

    /// Offline, the last entry seen at `path`, however old, keeps cached reads and writes
    /// working (C12).
    fn held(&self, path: &SpacePath) -> Option<(Entry, Instant, Option<Arc<File>>)> {
        let (fetched, _, entry) = lock(&self.recent).get(path)?;
        let local = self.journal.changes(entry.id);
        Some((entry, fetched, local))
    }

    /// Caches the entry, and the path of its inode if the kernel holds it, as current when
    /// `asked`.
    pub(super) fn remember(&self, entry: &Entry, (at, settled): Asked) {
        if let Some(known) = lock(&self.paths).get_mut(&self.ino(entry.id)) {
            (known.0, known.2) = (entry.path.clone(), Some(at));
        }
        let mut recent = lock(&self.recent);
        recent.insert(entry.path.clone(), 1, (at, settled, entry.clone()));
        recent.evict(RECENT, |_| false);
    }

    /// Caches an entry of a listing sent when `asked` as the next to go unless a lookup uses it,
    /// so a big listing does not push out the entries in use (C12).
    pub(super) fn listed(&self, entry: &Entry, (at, settled): Asked) {
        let mut recent = lock(&self.recent);
        recent.insert_cold(entry.path.clone(), 1, (at, settled, entry.clone()));
        recent.evict(RECENT, |_| false);
    }

    /// ESTALE for an entry the authority no longer finds where it was looked for. The entry
    /// cached there goes, so the lookup the kernel then makes asks the authority (C7).
    pub(super) fn gone(&self, path: &SpacePath, err: Error) -> Error {
        if !err.is_not_found() {
            return err;
        }
        lock(&self.recent).remove(path);
        Errno::ESTALE.into()
    }

    /// Counts a reply that hands the kernel a reference to the entry's inode, and returns its
    /// attributes for that reply. The entry, fetched at `fetched`, gives the inode its path
    /// unless the authority confirmed another since (C7).
    pub(super) fn looked_up(&self, entry: &Entry, fetched: Instant, local: Option<&File>) -> FileAttr {
        let mut paths = lock(&self.paths);
        let known = paths
            .entry(self.ino(entry.id))
            .or_insert((entry.path.clone(), 0, None));
        if known.2.is_none_or(|confirmed| confirmed < fetched) {
            (known.0, known.2) = (entry.path.clone(), Some(fetched));
        }
        known.1 += 1;
        drop(paths);
        self.attr(entry, local)
    }

    /// Records an entry this client just made, whose reply hands the kernel its inode.
    pub(super) fn added(&self, entry: &Entry) -> FileAttr {
        self.stale(&entry.path);
        let asked = self.asked();
        self.remember(entry, asked);
        self.looked_up(entry, asked.0, self.journal.changes(entry.id).as_deref())
    }

    /// Drops the kernel's `lookups` references to `ino`, and its path with the last one.
    pub fn forget(&self, ino: INodeNo, lookups: u64) {
        let mut paths = lock(&self.paths);
        let Some((_, count, _)) = paths.get_mut(&ino) else {
            return;
        };
        *count = count.saturating_sub(lookups);
        if *count == 0 && ino != INodeNo::ROOT {
            paths.remove(&ino);
        }
    }

    /// Drops the cached entries at or below `path`, which this client just added, moved or
    /// removed, and its directory's, whose mtime the authority moved with it. The kernel drops
    /// its own attributes of that directory after the change.
    pub(super) fn stale(&self, path: &SpacePath) {
        let parent = path.parent();
        lock(&self.recent).purge(|known| known.is_within(path) || Some(known) == parent.as_ref());
    }
}

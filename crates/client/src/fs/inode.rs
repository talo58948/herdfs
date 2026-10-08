//! What this client knows of one entry: its attributes as the authority last gave them, the
//! version its open files read, and its local changes, under one lock, so that every reply is
//! one snapshot of them (C1, C3) and journal I/O waits for no other entry.

use std::fs::File;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use herdfs_core::proto::{Entry, Version};

use super::OpenMode;
use crate::journal::Local;
use crate::{lock, reader::Head, version::OpenVersion};

pub struct Inode(Mutex<State>);

pub struct State {
    /// The entry as the authority last described it, and when that was asked for; `None` for
    /// an entry known only from a write recovered from the journal.
    entry: Option<Entry>,
    asked: Instant,
    /// When a commit of this client's last settled: attributes asked for before then are older.
    changed: Instant,
    /// Whether it may answer for its path within TTL: a change this client made says not.
    pub current: bool,
    /// The version its open files read, if known, and whether this client may write it.
    version: Option<Head>,
    writable: bool,
    /// This client's changes, while it has some or a handle is open (journal.rs).
    pub local: Option<Local>,
}

impl Inode {
    pub fn new(entry: Option<Entry>, asked: Instant) -> Self {
        Self(Mutex::new(State {
            entry,
            asked,
            changed: asked,
            current: true,
            version: None,
            writable: true,
            local: None,
        }))
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.0)
    }

    /// Whether it holds local state, or is busy, as journal I/O keeps it: then it stays.
    pub fn in_use(&self) -> bool {
        self.0.try_lock().map_or(true, |state| state.local.is_some())
    }
}

impl State {
    /// Takes the authority's description asked for at `asked`, unless one asked for later is
    /// held or a commit of this client settled since, whose result it may predate (C1).
    pub fn observe(&mut self, entry: &Entry, asked: Instant) {
        if asked >= self.asked && asked >= self.changed {
            (self.entry, self.asked, self.current) = (Some(entry.clone()), asked, true);
        }
    }

    /// The entry, when it was asked for, and whether it may still answer for its path.
    pub fn cached(&self) -> Option<(Entry, Instant, bool)> {
        Some((self.entry.clone()?, self.asked, self.current))
    }

    /// The entry with the journal copy of this client's changes, if any: what a reply shows.
    pub fn view(&self) -> Option<(Entry, Option<Arc<File>>)> {
        Some((self.entry.clone()?, self.local.as_ref().and_then(Local::copy)))
    }

    /// The version the entry's open files read, if known, and whether this client may write it.
    pub fn known(&self) -> Option<(Head, bool)> {
        Some((self.version.clone()?, self.writable))
    }

    /// The version the entry's open files read, if known and committed.
    pub fn followed(&self) -> Head {
        self.version.clone().flatten()
    }

    /// Whether open files of the entry read an older version than its head, and no local changes,
    /// which every read would see instead (C1, C3).
    pub fn behind(&self) -> bool {
        let local = self.local.as_ref();
        if !local.is_some_and(Local::is_open) || local.and_then(Local::copy).is_some() {
            return false;
        }
        let followed = self
            .version
            .as_ref()
            .map(|file| file.as_ref().map(|file| file.version.id));
        self.entry
            .as_ref()
            .is_some_and(|entry| followed < Some(entry.head))
    }

    /// Makes `head` the version the entry's open files read, from an open or a commit, unless a
    /// newer one is known, and records whether this client may write it. True if that replaced a
    /// version known before, whose pages the kernel may hold.
    pub fn switch(&mut self, (head, writable): (Option<Version>, bool)) -> bool {
        let older =
            |file: &Head| file.as_ref().map(|file| file.version.id) < head.as_ref().map(|head| head.id);
        let switched = self.version.as_ref().is_some_and(older);
        if self.version.as_ref().is_none_or(older) {
            self.version = Some(head.map(|head| Arc::new(OpenVersion::new(head))));
        }
        self.writable = writable;
        switched
    }

    /// The version open files read while the authority is unreachable: the head last followed,
    /// with the access it came with. A truncating write needs none, since its unguarded commit is
    /// right on an unknown base too, nor does one over pending changes, which hold the bytes (C5,
    /// C12). `None` if nothing is known to go on.
    pub(super) fn offline(&self, mode: OpenMode) -> Option<Result<Head, i32>> {
        match self.known() {
            Some((_, false)) if mode.writes() => Some(Err(libc::EACCES)),
            Some((file, _)) => Some(Ok(file)),
            None if mode.truncates() || self.local.as_ref().and_then(Local::copy).is_some() => Some(Ok(None)),
            None => None,
        }
    }

    /// Forgets the local changes once nothing holds them.
    pub fn tidy(&mut self) {
        if self.local.as_ref().is_some_and(Local::idle) {
            self.local = None;
        }
    }

    /// Settles a commit of this client's, in the critical section that drops its copy: the
    /// committed version, if any, becomes the entry's head with its size and time, and what the
    /// authority said of the entry before is older (C1).
    pub fn settle(&mut self, committed: Option<&Version>, mtime_ms: Option<i64>) -> bool {
        self.changed = Instant::now();
        let Some(version) = committed else {
            return false;
        };
        if let Some(entry) = &mut self.entry {
            (entry.head, entry.size) = (Some(version.id), version.size);
            entry.mtime_ms = mtime_ms.unwrap_or(entry.mtime_ms);
        }
        self.switch((Some(version.clone()), true))
    }
}

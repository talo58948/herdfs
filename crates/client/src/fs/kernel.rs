//! The kernel's side of the mount: the inode number of each entry, and the one way to drop what
//! the kernel cached of an inode (C3).

use std::sync::OnceLock;

use fuser::{INodeNo, Notifier};
use herdfs_core::proto::EntryId;

pub struct Kernel {
    root: EntryId,
    /// Set once mounted.
    notifier: OnceLock<Notifier>,
}

impl Kernel {
    pub fn new(root: EntryId) -> Self {
        let notifier = OnceLock::new();
        Self { root, notifier }
    }

    /// Inode numbers are entry ids plus one, except that the root is always inode 1.
    pub fn ino(&self, entry: EntryId) -> INodeNo {
        INodeNo(if entry == self.root { 1 } else { entry.0 + 1 })
    }

    pub fn entry(&self, ino: INodeNo) -> EntryId {
        EntryId(if ino.0 == 1 { self.root.0 } else { ino.0 - 1 })
    }

    pub fn mounted(&self, notifier: Notifier) {
        let _ = self.notifier.set(notifier);
    }

    /// Drops the kernel's pages and attributes of the entry's inode, whose bytes changed under
    /// it. With `wait`, returns once they are gone: a reply that switched the version waits, so
    /// no later read sees the old one (C3), and the kernel waits for the pages a read in progress
    /// holds, which other tasks answer. A read or a write, which hold pages themselves, and the
    /// committer let it run.
    pub async fn apply(&self, entry: EntryId, wait: bool) {
        let Some(notifier) = self.notifier.get().cloned() else {
            return;
        };
        let ino = self.ino(entry);
        let dropping = tokio::task::spawn_blocking(move || notifier.inval_inode(ino, 0, 0));
        if wait {
            let _ = dropping.await;
        }
    }
}

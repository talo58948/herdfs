//! The kernel's side of the mount: inode numbers and cached attributes.

use std::{ffi::OsStr, sync::OnceLock};

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

    /// Drops attributes cached before the entry switched versions. Direct I/O leaves no file
    /// content in the kernel to invalidate.
    pub fn invalidate_attrs(&self, entry: EntryId) {
        let Some(notifier) = self.notifier.get() else {
            return;
        };
        let ino = self.ino(entry);
        // A negative offset asks Linux to invalidate attributes without touching mapped pages.
        let _ = notifier.inval_inode(ino, -1, 0);
    }

    pub fn invalidate_entry(&self, parent: INodeNo, entry: EntryId, name: &OsStr) {
        if let Some(notifier) = self.notifier.get() {
            let _ = notifier.delete(parent, self.ino(entry), name);
        }
    }
}

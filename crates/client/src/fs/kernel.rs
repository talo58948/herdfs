//! The kernel's side of the mount: inode numbers and cached attributes.

use std::{ffi::OsStr, sync::OnceLock};

use herdfs_core::proto::EntryId;

use super::{InodeNo, Invalidation};

type Invalidator = Box<dyn Fn(Invalidation) + Send + Sync>;

pub struct Kernel {
    root: EntryId,
    /// Set once mounted.
    invalidator: OnceLock<Invalidator>,
}

impl Kernel {
    pub fn new(root: EntryId) -> Self {
        Self {
            root,
            invalidator: OnceLock::new(),
        }
    }

    pub fn root(&self) -> EntryId {
        self.root
    }

    /// Inode numbers are entry ids plus one, except that the root is always inode 1.
    pub fn ino(&self, entry: EntryId) -> InodeNo {
        InodeNo(if entry == self.root { 1 } else { entry.0 + 1 })
    }

    pub fn entry(&self, ino: InodeNo) -> EntryId {
        EntryId(if ino.0 == 1 { self.root.0 } else { ino.0 - 1 })
    }

    pub fn mounted(&self, invalidate: impl Fn(Invalidation) + Send + Sync + 'static) {
        let _ = self.invalidator.set(Box::new(invalidate));
    }

    /// Drops attributes cached before the entry switched versions. Direct I/O leaves no file
    /// content in the kernel to invalidate.
    pub fn invalidate_attrs(&self, entry: EntryId) {
        let Some(invalidate) = self.invalidator.get() else {
            return;
        };
        invalidate(Invalidation::Attributes(self.ino(entry)));
    }

    pub fn invalidate_entry(&self, parent: InodeNo, entry: EntryId, name: &OsStr) {
        if let Some(invalidate) = self.invalidator.get() {
            invalidate(Invalidation::Entry {
                parent,
                child: self.ino(entry),
                name: name.to_owned(),
            });
        }
    }
}

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use super::Space;
use crate::Result;

impl Space {
    pub async fn read(self: Arc<Self>, fh: u64, off: u64, len: u32) -> Result<Vec<u8>> {
        let handle = self.handle(fh)?;
        // Changes pending on this client show through every handle (C1).
        if let Some(copy) = self.changes(handle.entry) {
            return Ok(read_copy(&copy, off, len)?);
        }
        self.usable(handle.entry, |file| self.read_version(file, off, len, &handle))
            .await
    }
}

fn read_copy(copy: &File, off: u64, len: u32) -> io::Result<Vec<u8>> {
    let available = copy.metadata()?.len().saturating_sub(off);
    let mut bytes = vec![0; u64::from(len).min(available) as usize];
    copy.read_exact_at(&mut bytes, off)?;
    Ok(bytes)
}

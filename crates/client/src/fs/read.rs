use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use super::{Space, handles::Handle};
use crate::Result;
use crate::reader::Head;

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

    pub(super) async fn read_version(
        &self,
        file: Head,
        off: u64,
        len: u32,
        handle: &Handle,
    ) -> Result<Vec<u8>> {
        let Some(file) = file else {
            return Ok(Vec::new());
        };
        let access = handle.access.as_ref();
        let bytes = self
            .reader
            .read(&file, off, len.into(), access.map(|access| &access.bytes_fetched))
            .await?;
        if handle.sequential(off, bytes.len())
            && let Some(access) = access
        {
            self.read_ahead(file, off.saturating_add(bytes.len() as u64), access.clone());
        }
        Ok(bytes)
    }
}

fn read_copy(copy: &File, off: u64, len: u32) -> io::Result<Vec<u8>> {
    let available = copy.metadata()?.len().saturating_sub(off);
    let mut bytes = vec![0; u64::from(len).min(available) as usize];
    copy.read_exact_at(&mut bytes, off)?;
    Ok(bytes)
}

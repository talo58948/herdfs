use std::fs::File;
use std::os::unix::fs::FileExt;
use std::time::Instant;

use herdfs_core::proto::EntryId;

use super::{Flow, IDLE};
use crate::Result;
use crate::fs::nfs::Nfs;

impl Nfs {
    pub(in crate::fs::nfs) async fn revalidate_epoch(&self, entry: EntryId, state: &mut Flow) -> Result<()> {
        let Some(epoch) = &mut state.epoch else {
            return Ok(());
        };
        if epoch.checked.elapsed() < IDLE {
            return Ok(());
        }
        epoch.checked = Instant::now();
        match self.space.remote.locate(entry).await {
            Ok(_) => Ok(()),
            Err(error) if error.is_offline() => Ok(()),
            Err(error) => {
                let error = self.space.gone(self.space.ino(entry), error);
                if error.errno() == libc::ESTALE {
                    self.end_epoch(state).await?;
                }
                Err(error)
            }
        }
    }
}

pub(super) fn read_copy(copy: &File, off: u64, count: u32) -> std::io::Result<Vec<u8>> {
    let available = copy.metadata()?.len().saturating_sub(off);
    let mut bytes = vec![0; u64::from(count).min(available) as usize];
    copy.read_exact_at(&mut bytes, off)?;
    Ok(bytes)
}

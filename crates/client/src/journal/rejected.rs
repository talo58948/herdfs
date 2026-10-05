//! Rejected journal writes kept for manual recovery, within a fixed disk bound (C10).

use std::fs;
use std::io;

use super::{Journal, REJECTED, commit::Job, write_record};

/// How much the writes set aside in `rejected/` may hold.
const REJECTED_BYTES: u64 = 1 << 30;

impl Journal {
    /// Moves the record to `rejected/`, with a hard link that keeps its bytes.
    pub(super) fn reject(&self, job: &Job) {
        let record = &job.record;
        let name = format!("{}-{}", record.entry.0, record.request_id);
        let kept = self.dir.join(REJECTED).join(name);
        let result = write_record(&kept.with_extension("json"), record)
            .and_then(|()| fs::hard_link(&job.sealed, kept.with_extension("copy")))
            .and_then(|()| self.bound_rejected());
        if let Err(err) = result {
            tracing::warn!(%err, "could not keep a rejected write");
        }
    }

    /// Drops the oldest writes set aside once they hold more than `REJECTED_BYTES` (C10).
    fn bound_rejected(&self) -> io::Result<()> {
        let mut writes = Vec::new();
        for item in fs::read_dir(self.dir.join(REJECTED))? {
            let path = item?.path();
            if !path.extension().is_some_and(|extension| extension == "json") {
                continue;
            }
            let record = fs::metadata(&path)?;
            let copy = match fs::metadata(path.with_extension("copy")) {
                Ok(copy) => copy.len(),
                Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
                Err(err) => return Err(err),
            };
            writes.push((record.modified()?, record.len() + copy, path.with_extension("")));
        }
        writes.sort();
        let mut total: u64 = writes.iter().map(|(_, bytes, _)| bytes).sum();
        for (_, bytes, path) in writes {
            if total <= REJECTED_BYTES {
                break;
            }
            tracing::warn!(write = %path.display(), "dropping an old rejected write");
            let _ = fs::remove_file(path.with_extension("json"));
            let _ = fs::remove_file(path.with_extension("copy"));
            total = total.saturating_sub(bytes);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::time::{Duration, UNIX_EPOCH};

    use tokio::sync::{Notify, mpsc};

    use super::*;

    #[test]
    fn bounds_whole_writes_by_record_age() {
        // contract: C10
        let dir = tempfile::tempdir().unwrap();
        let rejected = dir.path().join(REJECTED);
        fs::create_dir(&rejected).unwrap();
        let (wake, woken) = mpsc::unbounded_channel();
        let journal = Journal {
            dir: dir.path().into(),
            wake,
            urged: Notify::new(),
        };
        drop(woken);
        let old = rejected.join("old");
        let new = rejected.join("new");
        for path in [&new, &old] {
            File::create(path.with_extension("copy"))
                .unwrap()
                .set_len(600 << 20)
                .unwrap();
        }
        for (path, age) in [(&old, 1), (&new, 2)] {
            let record = File::create(path.with_extension("json")).unwrap();
            record
                .set_modified(UNIX_EPOCH + Duration::from_secs(age))
                .unwrap();
        }

        journal.bound_rejected().unwrap();

        assert!(!old.with_extension("json").exists());
        assert!(!old.with_extension("copy").exists());
        assert!(new.with_extension("json").exists());
        assert!(new.with_extension("copy").exists());
    }
}

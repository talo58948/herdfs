//! Round 17: a chunk upload the authority records but the store fails to write.

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, ensure};
use herdfs_core::ChunkId;
use herdfs_lab::{ADMIN, Lab};

use crate::{VISIBLE_AFTER, random_bytes, save, two_clients};

/// The lab's bucket, a directory under MinIO's data directory.
const BUCKET: &str = "herdfs";
/// One chunk: below the smallest chunk size.
const LEN: usize = 64 << 10;
const READ_WITHIN: Duration = Duration::from_secs(30);

/// What became of a save made while the store refused the write of its one chunk.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// Whether a's `fsync` reported the commit acknowledged.
    fsynced: bool,
    /// Whether the bucket holds the head's chunk once a has nothing left to commit.
    stored: bool,
    /// Whether b then reads exactly the saved bytes.
    read: bool,
}

/// Whether b reads exactly `expected` at `name` within [`READ_WITHIN`].
fn b_reads(lab: &Lab, name: &str, expected: &[u8]) -> bool {
    let (done, got) = mpsc::channel();
    let path = lab.mount_path("b").join(name);
    // Detached, so that a read the client never answers fails the test instead of hanging it.
    thread::spawn(move || done.send(fs::read(path)));
    got.recv_timeout(READ_WITHIN)
        .is_ok_and(|got| got.is_ok_and(|got| got == expected))
}

/// a saves a warm-up file, so the bucket has its `chunks/` directory on MinIO's disk. With
/// `refuse`, the test then puts an empty directory without write permission where MinIO keeps
/// the next file's one chunk, so the store fails that object's write (a stand-in for any store
/// error on one upload). a saves the file with fsync; the test restores the directory, waits
/// until a has nothing left to commit, checks that b reads the warm-up file (the store is
/// healthy), and has b read the file.
fn save_while_the_store_refuses(refuse: bool) -> Result<Option<Outcome>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    let warm = random_bytes(LEN);
    save(&lab.mount_path("a").join("warm"), &warm)?;
    let bytes = random_bytes(LEN);
    let id = ChunkId::of(&bytes);
    let blocker = lab.store_dir().join(BUCKET).join("chunks").join(id.to_string());
    if refuse {
        fs::create_dir(&blocker)?;
        fs::set_permissions(&blocker, Permissions::from_mode(0o555))?;
    }
    let fsynced = save(&lab.mount_path("a").join("f"), &bytes).is_ok();
    if refuse {
        fs::set_permissions(&blocker, Permissions::from_mode(0o755))?;
        fs::remove_dir(&blocker)?;
    }
    lab.wait_committed("a")?;
    thread::sleep(VISIBLE_AFTER);
    ensure!(b_reads(&lab, "warm", &warm), "b reads the warm-up file");
    let objects = lab.stored("chunks/")?;
    let head = lab.head(ADMIN, "f")?;
    let stored = head
        .chunks
        .iter()
        .all(|chunk| objects.contains_key(&format!("chunks/{}", chunk.id)));
    Ok(Some(Outcome {
        fsynced,
        stored,
        read: b_reads(&lab, "f", &bytes),
    }))
}

/// [`save_while_the_store_refuses`] with the store refusing. The upload handler used to record
/// the upload before it wrote the object and keep the record when the write failed, so the
/// commit went through and named a chunk the store never got: fsync succeeded and b's read
/// failed for good. It now records only once the store holds the bytes (authority
/// server/mod.rs `upload`). The control is [`a_save_while_the_store_writes_is_readable_elsewhere`].
// contract: C4a, C4b, C14
#[test]
fn a_save_while_the_store_refuses_is_readable_elsewhere() -> Result<()> {
    let Some(outcome) = save_while_the_store_refuses(true)? else {
        return Ok(());
    };
    assert!(
        outcome.stored && outcome.read,
        "the head's chunk is stored and b reads the saved bytes: {outcome:?}"
    );
    Ok(())
}

/// Control: the same save without the store's refusal.
// contract: C4a, C4b, C14
#[test]
fn a_save_while_the_store_writes_is_readable_elsewhere() -> Result<()> {
    let Some(outcome) = save_while_the_store_refuses(false)? else {
        return Ok(());
    };
    assert_eq!(
        outcome,
        Outcome {
            fsynced: true,
            stored: true,
            read: true
        }
    );
    Ok(())
}

//! Two clients creating the same new file at once (C5).

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::Barrier;
use std::thread;

use anyhow::Result;

use crate::two_clients;

const ROUNDS: usize = 20;

/// Repro: a and b each open the same new name with O_CREAT (no O_EXCL) at the same moment and
/// write. Each kernel looks the name up, misses, and sends CREATE; the authority answers the
/// later one `Exists`, which reaches the program as EEXIST, so that save never commits.
// contract: C5
#[test]
#[ignore = "REDTEAM: concurrent O_CREAT of one new name on two clients fails one with EEXIST"]
fn concurrent_creates_of_one_name_both_save() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let barrier = Barrier::new(2);
    let save = |mount: &Path, round: usize| {
        barrier.wait();
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(mount.join(format!("new-{round}")));
        file.and_then(|mut file| file.write_all(b"saved"))
    };
    let mut refused = 0;
    for round in 0..ROUNDS {
        let outcomes = thread::scope(|scope| {
            let saves = ["a", "b"].map(|name| scope.spawn(|| save(lab.mount_path(name), round)));
            saves.map(|save| save.join().map_err(|_| anyhow::anyhow!("a save panicked")))
        });
        for outcome in outcomes {
            match outcome? {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => refused += 1,
                Err(error) => return Err(error.into()),
            }
        }
    }
    assert_eq!(refused, 0, "saves refused with EEXIST in {ROUNDS} rounds");
    Ok(())
}

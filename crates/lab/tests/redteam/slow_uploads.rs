//! Round 16: a write whose chunks take longer to upload than the collection grace. The client
//! uploads every chunk of the file, then commits; collection must not forget the first uploads
//! meanwhile, or the commit is refused and the write set aside.

use std::fs;
use std::time::Duration;

use anyhow::Result;
use herdfs_lab::{Collection, Lab, LabConfig, Profile, Strategy};

use crate::{random_bytes, save};

/// About 17 s at `SLOW`, over three times the quick grace.
const LEN: usize = 16 << 20;
const TRIALS: usize = 3;
/// 1 MB/s each way.
const SLOW: Profile = Profile::Custom {
    round_trip_ms: 2,
    kilobytes_per_sec: 1_000,
    store_first_byte_ms: 0,
};

/// In each trial, a saves a new `LEN`-byte file over `SLOW` links while the authority collects
/// with `grace`. Returns, per trial, `committed` if the save (which fsyncs) succeeded and the
/// head at the authority is the saved size, else the save's error; and how many commits a's log
/// shows refused because a chunk was "neither in the base nor uploaded".
fn save_slowly(grace: Duration) -> Result<Option<(Vec<String>, usize)>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let collection = Collection {
        grace,
        ..Collection::QUICK
    };
    let config = LabConfig::default()
        .client("a", "alice", &[], SLOW, Strategy::Stream)
        .collection(collection);
    let lab = Lab::start(config)?;
    let mut outcomes = Vec::new();
    for trial in 0..TRIALS {
        let name = format!("big{trial}");
        let saved = save(&lab.mount_path("a").join(&name), &random_bytes(LEN));
        let head = lab.head("alice", &name).map(|version| version.size);
        outcomes.push(match saved {
            Ok(()) if head.ok() == Some(LEN as u64) => "committed".to_owned(),
            Ok(()) => "saved, but the head is another size".to_owned(),
            Err(err) => err.to_string(),
        });
    }
    let log = fs::read_to_string(lab.client_dir("a").join("client.log"))?;
    let refused = log.matches("neither in the base nor uploaded").count();
    Ok(Some((outcomes, refused)))
}

/// [`save_slowly`] with the quick grace of 5 s. Collection used to delete each upload record
/// once it was older than the grace, so the commit was `Invalid` ("neither in the base nor
/// uploaded by you") and set aside, and `fsync` failed with EIO. At the contract's 1 h grace that
/// hit any file whose upload took over an hour, e.g. 25 GiB at 50 Mbit/s. The control is
/// [`a_save_that_uploads_within_the_grace_commits`].
// contract: C4a
#[test]
fn a_save_that_uploads_for_longer_than_the_grace_commits() -> Result<()> {
    let Some((outcomes, refused)) = save_slowly(Collection::QUICK.grace)? else {
        return Ok(());
    };
    assert_eq!(
        outcomes, ["committed"; TRIALS],
        "each slow save; {refused} commits refused"
    );
    Ok(())
}

/// Control: the same saves with a grace longer than their uploads.
// contract: C4a
#[test]
fn a_save_that_uploads_within_the_grace_commits() -> Result<()> {
    let Some((outcomes, refused)) = save_slowly(Duration::from_secs(120))? else {
        return Ok(());
    };
    assert_eq!(
        outcomes, ["committed"; TRIALS],
        "each slow save; {refused} commits refused"
    );
    Ok(())
}

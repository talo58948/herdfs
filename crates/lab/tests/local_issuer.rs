//! A mount authenticated only by the authority's built-in issuer.

use std::fs;

use anyhow::Result;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

#[test]
fn a_locally_issued_token_mounts_and_writes() -> Result<()> {
    // contract: S5, C1
    let lab = Lab::start(
        LabConfig::default()
            .local_issuer()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream),
    )?;
    let file = lab.mount_path("a").join("local-token");
    fs::write(&file, b"self hosted")?;
    assert_eq!(fs::read(file)?, b"self hosted");
    Ok(())
}

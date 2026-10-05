//! Round 43: grant reuse at expiry and layered journal recovery after the client polish pass.

use std::fs::{self, File};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use herdfs_lab::{ADMIN, Lab, LabConfig, Links, Profile, Strategy};

use crate::{VISIBLE_AFTER, random_bytes, read_at, save, two_clients, wait_pending};

const TRIALS: usize = 3;

fn counter(lab: &Lab, name: &str) -> Result<u64> {
    lab.stats("b")?[name]
        .as_u64()
        .with_context(|| format!("stats count {name}"))
}

fn read_starts(file: &File, starts: &[(u64, u8)]) -> Result<()> {
    thread::scope(|scope| {
        let reads = starts
            .iter()
            .map(|&(offset, expected)| scope.spawn(move || (read_at(file, offset, 1), expected)))
            .collect::<Vec<_>>();
        for read in reads {
            let (bytes, expected) = read.join().map_err(|_| anyhow::anyhow!("read thread panicked"))?;
            ensure!(bytes? == [expected], "wrong byte at a chunk boundary");
        }
        Ok(())
    })
}

/// With no cache capacity, three concurrent reads must download again while reusing their live
/// grant URLs, then renew those URLs before using them after the 60-second grant TTL.
// contract: C9, S2
#[test]
fn concurrent_reads_reuse_live_grants_and_renew_expired_ones() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
        .cache_bytes("b", 0);
    let lab = Lab::start(config)?;
    // More than two maximum-size chunks, so all three concurrent reads always exist.
    let bytes = random_bytes(9 << 20);
    save(&lab.mount_path("a").join("grants"), &bytes)?;
    let version = lab.head(ADMIN, "grants")?;
    ensure!(version.chunks.len() >= TRIALS, "test file has too few chunks");
    let mut offset = 0_u64;
    let starts = version
        .chunks
        .iter()
        .take(TRIALS)
        .map(|chunk| {
            let start = (offset, bytes[offset as usize]);
            offset += u64::from(chunk.len);
            start
        })
        .collect::<Vec<_>>();
    thread::sleep(VISIBLE_AFTER);
    let file = File::open(lab.mount_path("b").join("grants"))?;

    read_starts(&file, &starts)?;
    let first = counter(&lab, "grants")?;
    ensure!(first > 0, "cold reads requested no grants");
    ensure!(
        counter(&lab, "chunks_fetched")? == TRIALS as u64,
        "cold fetch count"
    );
    read_starts(&file, &starts)?;
    ensure!(
        counter(&lab, "grants")? == first,
        "live grant URLs were not reused"
    );
    ensure!(
        counter(&lab, "chunks_fetched")? == (2 * TRIALS) as u64,
        "warm fetch count"
    );

    thread::sleep(Duration::from_secs(61));
    read_starts(&file, &starts)?;
    ensure!(
        counter(&lab, "grants")? > first,
        "expired grant URLs were not renewed"
    );
    ensure!(
        counter(&lab, "chunks_fetched")? == (3 * TRIALS) as u64,
        "renewed fetch count"
    );
    Ok(())
}

fn layer_truncate(lab: &Lab, name: &str) -> Result<()> {
    fs::write(lab.mount_path("b").join(name), b"pending rewrite")?;
    wait_pending(lab)?;
    drop(File::create(lab.mount_path("b").join(name))?);
    Ok(())
}

/// A truncate-only close supersedes an already-sealed offline rewrite. A reconnecting control
/// and three immediate client crashes must all commit the latest, empty copy.
// contract: C3, C4a, C12
#[test]
fn truncate_over_a_pending_seal_survives_an_immediate_crash() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let a = lab.mount_path("a").to_owned();
    let b = lab.mount_path("b").to_owned();

    save(&a.join("layered-control"), b"old")?;
    thread::sleep(VISIBLE_AFTER);
    fs::metadata(b.join("layered-control"))?;
    lab.cut("b", Links::Authority)?;
    layer_truncate(&lab, "layered-control")?;
    lab.restore("b", Links::Authority)?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    ensure!(
        fs::read(a.join("layered-control"))?.is_empty(),
        "control truncate"
    );

    for trial in 0..TRIALS {
        let name = format!("layered-crash-{trial}");
        save(&a.join(&name), b"old")?;
        thread::sleep(VISIBLE_AFTER);
        fs::metadata(b.join(&name))?;
        lab.cut("b", Links::Authority)?;
        layer_truncate(&lab, &name)?;
        lab.kill_client("b")?;
        lab.restore("b", Links::Authority)?;
        lab.restart_client("b")?;
        lab.wait_committed("b")?;
        thread::sleep(VISIBLE_AFTER);
        ensure!(fs::read(a.join(name))?.is_empty(), "trial {trial} truncate");
    }
    Ok(())
}

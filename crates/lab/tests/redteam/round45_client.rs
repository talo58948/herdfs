//! Round 45: layered journal copies and concurrent publication of identical cached chunks.

use std::fs::{self, File};
use std::io::Write;
use std::sync::Barrier;
use std::thread;

use anyhow::{Result, ensure};
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy};

use crate::{VISIBLE_AFTER, assert_bytes, random_bytes, save, two_clients, wait_pending};

const TRIALS: usize = 3;

fn layer_append(lab: &Lab, name: &str) -> Result<()> {
    let path = lab.mount_path("b").join(name);
    fs::write(&path, b"first rewrite")?;
    wait_pending(lab)?;
    let mut file = File::options().append(true).open(path)?;
    file.write_all(b" plus second")?;
    drop(file);
    wait_pending(lab)
}

/// An append over an already sealed offline rewrite starts a new working copy from that seal.
/// A reconnecting control and three immediate crashes must commit both layers.
// contract: C3, C4a, C12
#[test]
fn append_over_a_pending_seal_survives_an_immediate_crash() -> Result<()> {
    let Some(mut lab) = two_clients()? else {
        return Ok(());
    };
    let a = lab.mount_path("a").to_owned();
    let b = lab.mount_path("b").to_owned();
    let expected = b"first rewrite plus second";

    save(&a.join("append-control"), b"old")?;
    thread::sleep(VISIBLE_AFTER);
    fs::metadata(b.join("append-control"))?;
    lab.cut("b", Links::Authority)?;
    layer_append(&lab, "append-control")?;
    lab.restore("b", Links::Authority)?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(&fs::read(a.join("append-control"))?, expected, "control append");

    for trial in 0..TRIALS {
        let name = format!("append-crash-{trial}");
        save(&a.join(&name), b"old")?;
        thread::sleep(VISIBLE_AFTER);
        fs::metadata(b.join(&name))?;
        lab.cut("b", Links::Authority)?;
        layer_append(&lab, &name)?;
        lab.kill_client("b")?;
        lab.restore("b", Links::Authority)?;
        lab.restart_client("b")?;
        lab.wait_committed("b")?;
        thread::sleep(VISIBLE_AFTER);
        assert_bytes(&fs::read(a.join(name))?, expected, "recovered layered append");
    }
    Ok(())
}

fn concurrent_copy(lab: &Lab, name: &str, bytes: &[u8], concurrent: bool) -> Result<()> {
    let source = lab.mount_path("b").join("cache-source");
    let destination = lab.mount_path("b").join(name);
    if !concurrent {
        assert_bytes(&fs::read(&source)?, bytes, "sequential source");
        save(&destination, bytes)?;
        assert_bytes(&fs::read(destination)?, bytes, "sequential copy");
        return Ok(());
    }
    let start = Barrier::new(2);
    thread::scope(|scope| -> Result<()> {
        let reader = scope.spawn(|| {
            start.wait();
            fs::read(source)
        });
        start.wait();
        save(&destination, bytes)?;
        let read = reader.join().map_err(|_| anyhow::anyhow!("reader panicked"))??;
        assert_bytes(&read, bytes, "source during identical commit");
        Ok(())
    })?;
    assert_bytes(&fs::read(destination)?, bytes, "committed identical copy");
    Ok(())
}

/// A download and a commit on one client can publish the same verified chunk concurrently. A
/// sequential control and three races must keep correct bytes and leave no cache overrun.
// contract: C1, C10, C11
#[test]
fn concurrent_identical_chunk_publication_stays_coherent_and_bounded() -> Result<()> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    const CACHE_BYTES: u64 = 5 << 20;
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Wan, Strategy::Stream)
        .cache_bytes("b", CACHE_BYTES);
    let lab = Lab::start(config)?;
    let bytes = random_bytes(9 << 20);
    save(&lab.mount_path("a").join("cache-source"), &bytes)?;
    thread::sleep(VISIBLE_AFTER);

    concurrent_copy(&lab, "cache-control", &bytes, false)?;
    for trial in 0..TRIALS {
        concurrent_copy(&lab, &format!("cache-race-{trial}"), &bytes, true)?;
    }
    thread::sleep(VISIBLE_AFTER);
    let cached = fs::read_dir(lab.client_dir("b").join("cache"))?
        .map(|entry| Ok(entry?.metadata()?.len()))
        .sum::<Result<u64>>()?;
    ensure!(cached <= CACHE_BYTES, "cache holds {cached} bytes");
    Ok(())
}

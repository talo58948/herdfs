//! Focused loopback NFSv3 behavior on the Linux system client.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{EntryId, Request, Response};
use herdfs_lab::{Lab, LabConfig, Links, Mount, Profile, SPACE, Strategy};

const IDLE: Duration = Duration::from_millis(1_200);

fn lab() -> Result<Lab> {
    Lab::start(
        LabConfig::default()
            .mount(Mount::Nfs)
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )
}

fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn pending(client: &Path) -> Result<bool> {
    let records = fs::read_dir(client.join("journal"))?;
    Ok(records.filter_map(|entry| entry.ok()).any(|entry| {
        entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
    }))
}

fn unstable_write(client: &Path, entry: EntryId, data: &[u8]) -> Result<()> {
    let identity: serde_json::Value = serde_json::from_slice(&fs::read(client.join("journal.nfs.json"))?)?;
    let port = identity["port"].as_u64().unwrap_or_default() as u16;
    let export = identity["export_id"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|byte| byte.as_u64().map(|byte| byte as u8));
    let mut handle = b"OSNF".to_vec();
    handle.extend_from_slice(&1_u32.to_be_bytes());
    handle.extend(export);
    handle.extend_from_slice(&entry.0.to_be_bytes());
    ensure!(handle.len() == 32, "invalid persisted export id");

    let mut call = Vec::new();
    for word in [1_u32, 0, 2, 100_003, 3, 7, 0, 0, 0, 0] {
        call.extend_from_slice(&word.to_be_bytes());
    }
    opaque(&mut call, &handle);
    call.extend_from_slice(&0_u64.to_be_bytes());
    call.extend_from_slice(&(data.len() as u32).to_be_bytes());
    call.extend_from_slice(&0_u32.to_be_bytes());
    opaque(&mut call, data);

    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.write_all(&((call.len() as u32) | (1 << 31)).to_be_bytes())?;
    stream.write_all(&call)?;
    let mut marker = [0; 4];
    stream.read_exact(&mut marker)?;
    let len = (u32::from_be_bytes(marker) & !(1 << 31)) as usize;
    let mut reply = vec![0; len];
    stream.read_exact(&mut reply)?;
    let status = reply.get(24..28).and_then(|bytes| bytes.try_into().ok());
    ensure!(
        status.map(u32::from_be_bytes) == Some(0),
        "NFS WRITE failed: {reply:?}"
    );
    Ok(())
}

fn opaque(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    out.resize(out.len().next_multiple_of(4), 0);
}

// contract: M2, P1, C2
#[test]
fn commit_is_a_barrier_and_reads_advance_only_after_epoch_idle() -> Result<()> {
    let lab = lab()?;
    let a = lab.mount_path("a").join("epoch");
    let b = lab.mount_path("b").join("epoch");
    save(&a, b"first")?;
    ensure!(
        !pending(lab.client_dir("a"))?,
        "fsync returned with a pending commit"
    );
    thread::sleep(IDLE);
    assert_eq!(fs::read(&b)?, b"first");

    save(&a, b"second")?;
    assert_eq!(fs::read(&b)?, b"first", "active epoch changed versions");
    thread::sleep(IDLE);
    assert_eq!(fs::read(&b)?, b"second");
    Ok(())
}

// contract: M2, C4a
#[test]
fn an_uncommitted_write_is_sealed_after_one_second_idle() -> Result<()> {
    let lab = lab()?;
    let path = lab.mount_path("a").join("idle");
    save(&path, b"x")?;
    let response = lab.rpc(
        "alice",
        &Request::Stat {
            space: SPACE.into(),
            path: SpacePath::try_from("idle").unwrap(),
        },
    )?;
    let Response::Entry { entry } = response else {
        anyhow::bail!("stat returned {response:?}");
    };
    unstable_write(lab.client_dir("a"), entry.id, b"sealed while open")?;
    thread::sleep(Duration::from_secs(2));
    lab.wait_committed("a")?;
    thread::sleep(IDLE);
    assert_eq!(fs::read(lab.mount_path("b").join("idle"))?, b"sealed while open");
    Ok(())
}

// contract: M2, C12
#[test]
fn offline_close_returns_after_the_sealed_write_is_journaled() -> Result<()> {
    let lab = lab()?;
    let path = lab.mount_path("a").join("offline");
    save(&path, b"online")?;
    thread::sleep(IDLE);
    lab.silence("a", Links::Authority)?;
    let _ = fs::metadata(&path)?;

    let started = Instant::now();
    let mut file = File::create(&path)?;
    file.write_all(b"offline")?;
    drop(file);
    ensure!(
        started.elapsed() < Duration::from_secs(2),
        "offline close blocked"
    );
    ensure!(
        pending(lab.client_dir("a"))?,
        "offline close left no journal record"
    );

    lab.restore("a", Links::Authority)?;
    lab.wait_committed("a")?;
    thread::sleep(IDLE);
    assert_eq!(fs::read(lab.mount_path("b").join("offline"))?, b"offline");
    Ok(())
}

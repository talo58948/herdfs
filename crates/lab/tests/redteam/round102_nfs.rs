//! Round 102: duplicate NFS mutations remain replay-safe while every replay slot is in progress.

use std::net::TcpStream;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, EntryId, Request, Response};
use herdfs_lab::{Lab, LabConfig, Links, Profile, SPACE, Strategy};

use crate::nfs_support::protocol::{rpc_header, send_record};
use crate::nfs_support::socket::nfs_stream;
use crate::nfs_support::{Rpc, opaque};
use crate::save;

const REPLAY_LIMIT: usize = 4_096;
const FLOOD_REQUESTS: usize = REPLAY_LIMIT + 128;
const FILE_LIMIT: u64 = 16_384;

struct FileLimit {
    soft: u64,
    hard: u64,
}

impl FileLimit {
    fn raise() -> Result<Self> {
        let pid = std::process::id().to_string();
        let output = Command::new("prlimit")
            .args([
                "--pid",
                &pid,
                "--nofile",
                "--output",
                "SOFT,HARD",
                "--raw",
                "--noheadings",
            ])
            .output()?;
        ensure!(output.status.success(), "could not read the file limit");
        let text = String::from_utf8(output.stdout)?;
        let mut fields = text.split_whitespace();
        let soft = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("file limit has no soft value"))?
            .parse()?;
        let hard = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("file limit has no hard value"))?
            .parse()?;
        ensure!(hard >= FILE_LIMIT, "hard file limit is below {FILE_LIMIT}");
        // The lab children inherit this test's limit; the guard restores it after they exit.
        let status = Command::new("prlimit")
            .args(["--pid", &pid, &format!("--nofile={FILE_LIMIT}:{hard}")])
            .status()?;
        ensure!(status.success(), "could not raise the file limit");
        Ok(Self { soft, hard })
    }
}

impl Drop for FileLimit {
    fn drop(&mut self) {
        let _ = Command::new("prlimit")
            .args([
                "--pid",
                &std::process::id().to_string(),
                &format!("--nofile={}:{}", self.soft, self.hard),
            ])
            .status();
    }
}

fn flood_commits(client: &std::path::Path, rpc: &Rpc) -> Result<TcpStream> {
    let identity: serde_json::Value =
        serde_json::from_slice(&std::fs::read(client.join("journal.nfs.json"))?)?;
    let port = identity["port"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("NFS identity has no port"))? as u16;
    let mut stream = nfs_stream(port)?;
    for offset in 0..FLOOD_REQUESTS {
        let handle = rpc.handle(EntryId(9_000_000_000 + offset as u64));
        let mut call = rpc_header(offset as u32 + 1, 21, 1);
        opaque(&mut call, &handle);
        call.extend_from_slice(&0_u64.to_be_bytes());
        call.extend_from_slice(&0_u32.to_be_bytes());
        send_record(&mut stream, &call)?;
    }
    Ok(stream)
}

fn entry(lab: &Lab, name: &str) -> Result<Entry> {
    let response = lab.rpc(
        "alice",
        &Request::Stat {
            space: SPACE.into(),
            path: SpacePath::try_from(name)?,
        },
    )?;
    let Response::Entry { entry } = response else {
        anyhow::bail!("stat returned {response:?}");
    };
    Ok(entry)
}

/// A completed duplicate REMOVE controls three duplicate pairs sent while 4,096 unrelated
/// COMMITs are in progress. Every pair must replay one execution and return the same success.
// contract: M1
#[test]
#[ignore = "ROUND102: all-in-progress replay saturation re-executes duplicate mutations"]
fn round102_in_progress_replay_saturation_keeps_duplicate_mutations_idempotent() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let _limit = FileLimit::raise()?;
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let rpc = Rpc::open(lab.client_dir("a"))?;
    let root = rpc.handle(entry(&lab, "")?.id);

    save(&lab.mount_path("a").join("replay-control"), b"control")?;
    let control = [
        rpc.remove_xid(&root, "replay-control", 0x1020_0000)?,
        rpc.remove_xid(&root, "replay-control", 0x1020_0000)?,
    ];
    ensure!(
        control == [0, 0],
        "completed duplicate REMOVE control was not replayed"
    );
    eprintln!("control: completed duplicate REMOVE statuses={control:?}");
    for trial in 0..3 {
        save(
            &lab.mount_path("a").join(format!("saturated-remove-{trial}")),
            b"target",
        )?;
    }

    lab.silence("a", Links::Authority)?;
    let flood = flood_commits(lab.client_dir("a"), &rpc)?;
    thread::sleep(Duration::from_secs(1));

    let mut workers = Vec::new();
    for trial in 0..3 {
        for _ in 0..2 {
            let client = lab.client_dir("a").to_path_buf();
            let root = root.clone();
            workers.push(thread::spawn(move || {
                Rpc::open(&client)?.remove_xid(
                    &root,
                    &format!("saturated-remove-{trial}"),
                    0x1021_0000 + trial as u32,
                )
            }));
        }
    }
    thread::sleep(Duration::from_millis(250));
    lab.restore("a", Links::Authority)?;

    let mut failures = 0;
    let mut workers = workers.into_iter();
    for trial in 0..3 {
        let first = workers
            .next()
            .expect("each trial has two workers")
            .join()
            .map_err(|_| anyhow::anyhow!("REMOVE worker panicked"))??;
        let second = workers
            .next()
            .expect("each trial has two workers")
            .join()
            .map_err(|_| anyhow::anyhow!("REMOVE worker panicked"))??;
        let replayed = first == 0 && second == 0;
        eprintln!("trial {trial}: duplicate REMOVE statuses=[{first}, {second}], replayed={replayed}");
        failures += usize::from(!replayed);
    }
    drop(flood);
    assert_eq!(failures, 0, "duplicate removals re-executed, of 3");
    Ok(())
}

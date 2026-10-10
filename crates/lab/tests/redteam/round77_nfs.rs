//! Round 77: NFS read epochs and loopback caller isolation.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::{Rpc, denied_auth};
use crate::save;

const NFS3ERR_STALE: u32 = 70;
const NFS3ERR_BADHANDLE: u32 = 10_001;
const NFS_PROGRAM: u32 = 100_003;
const MOUNT_PROGRAM: u32 = 100_005;
const VISIBLE: Duration = Duration::from_millis(1_200);

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

/// A live file controls three cached read epochs kept active after another client deletes their
/// entries. Once deletion has been visible for T_vis, use of each unlinked handle must be stale.
// contract: C7, M1
#[test]
fn round77_active_read_epoch_does_not_hide_a_deleted_handle() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )?;
    save(&lab.mount_path("a").join("live-control"), b"control")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let control = rpc.handle(entry(&lab, "live-control")?.id);
    let (status, bytes) = rpc.read(&control, 0, 7)?;
    ensure!(status == 0 && bytes == b"control", "live-handle control failed");
    let control_deadline = Instant::now() + VISIBLE;
    while Instant::now() < control_deadline {
        let (status, bytes) = rpc.read(&control, 0, 7)?;
        ensure!(status == 0 && bytes == b"control", "live-handle control failed");
        thread::sleep(Duration::from_millis(200));
    }

    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("deleted-active-read-{trial}");
        let bytes = format!("cached-{trial}").into_bytes();
        save(&lab.mount_path("a").join(&name), &bytes)?;
        let handle = rpc.handle(entry(&lab, &name)?.id);
        let (status, primed) = rpc.read(&handle, 0, bytes.len() as u32)?;
        ensure!(status == 0 && primed == bytes, "trial {trial} prime failed");
        fs::remove_file(lab.mount_path("b").join(&name))?;

        let deadline = Instant::now() + VISIBLE;
        let (status, observed) = loop {
            let result = rpc.read(&handle, 0, bytes.len() as u32)?;
            if result.0 == NFS3ERR_STALE || Instant::now() >= deadline {
                break result;
            }
            ensure!(result.0 == 0, "trial {trial} READ returned {}", result.0);
            thread::sleep(Duration::from_millis(200));
        };
        eprintln!("trial {trial}: read after deletion returned status={status}, bytes={observed:?}");
        failures += usize::from(status != NFS3ERR_STALE);
    }
    assert_eq!(failures, 0, "deleted filehandles still readable, of 3");
    Ok(())
}

/// An unprivileged caller claims the mounting uid but cannot obtain the target export's random
/// filehandle prefix: MOUNT rejects its source port and NFS rejects another export's handle.
// contract: M1, S3
#[test]
fn round77_loopback_export_is_limited_to_the_mounting_user() -> Result<()> {
    if let Ok(port) = std::env::var("ROUND77_NFS_PORT") {
        let uid = std::env::var("ROUND77_NFS_UID")?.parse()?;
        ensure!(
            rustix::process::geteuid().as_raw() != uid,
            "helper still has the mounting uid"
        );
        let port = port.parse()?;
        let foreign: Vec<u8> = serde_json::from_str(&std::env::var("ROUND77_NFS_HANDLE")?)?;

        let mut mount = Vec::new();
        opaque(&mut mount, b"/");
        ensure!(
            denied_auth(&call(port, MOUNT_PROGRAM, 3, 1, uid, &mount)?)?,
            "unprivileged MOUNT was accepted"
        );

        let mut getattr = Vec::new();
        opaque(&mut getattr, &foreign);
        let mut reply = Reply::new(call(port, NFS_PROGRAM, 3, 1, uid, &getattr)?)?;
        ensure!(
            reply.word()? == NFS3ERR_BADHANDLE,
            "foreign filehandle was accepted"
        );
        return Ok(());
    }
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("b", "bob", &[], Profile::Lan, Strategy::Stream),
    )?;
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(lab.client_dir("a").join("journal.nfs.json"))?)?;
    let port = identity["port"].as_u64().context("NFS identity has no port")? as u16;
    let foreign_identity: serde_json::Value =
        serde_json::from_slice(&fs::read(lab.client_dir("b").join("journal.nfs.json"))?)?;
    let export = foreign_identity["export_id"]
        .as_array()
        .context("foreign NFS identity has no export id")?;
    let mut foreign = b"OSNF".to_vec();
    foreign.extend_from_slice(&1_u32.to_be_bytes());
    foreign.extend(export.iter().map(|byte| byte.as_u64().unwrap_or_default() as u8));
    foreign.extend_from_slice(&1_u64.to_be_bytes());
    ensure!(foreign.len() == 32, "foreign NFS identity is invalid");
    save(&lab.mount_path("a").join("local-user-control"), b"control")?;
    ensure!(fs::read(lab.mount_path("a").join("local-user-control"))? == b"control");

    let helper_dir = tempfile::tempdir()?;
    fs::set_permissions(helper_dir.path(), fs::Permissions::from_mode(0o755))?;
    let executable = helper_dir.path().join("redteam");
    fs::copy(std::env::current_exe()?, &executable)?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
    let output = Command::new("sudo")
        .args(["-n", "-u", "nobody", "env"])
        .arg(format!("ROUND77_NFS_PORT={port}"))
        .arg(format!("ROUND77_NFS_UID={}", rustix::process::geteuid().as_raw()))
        .arg(format!("ROUND77_NFS_HANDLE={}", serde_json::to_string(&foreign)?))
        .arg(&executable)
        .args([
            "--exact",
            "round77_nfs::round77_loopback_export_is_limited_to_the_mounting_user",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "unprivileged helper failed ({}): {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn call(port: u16, program: u32, version: u32, procedure: u32, uid: u32, args: &[u8]) -> Result<Vec<u8>> {
    let mut call = Vec::new();
    for word in [
        0x7700_0001,
        0,
        2,
        program,
        version,
        procedure,
        1,
        20,
        0,
        0,
        uid,
        uid,
        0,
        0,
        0,
    ] {
        call.extend_from_slice(&word.to_be_bytes());
    }
    call.extend_from_slice(args);
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.write_all(&((call.len() as u32 | 1 << 31).to_be_bytes()))?;
    stream.write_all(&call)?;
    let mut marker = [0; 4];
    stream.read_exact(&mut marker)?;
    let mut reply = vec![0; (u32::from_be_bytes(marker) & !(1 << 31)) as usize];
    stream.read_exact(&mut reply)?;
    Ok(reply)
}

fn opaque(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    out.resize(out.len().next_multiple_of(4), 0);
}

struct Reply {
    bytes: Vec<u8>,
    at: usize,
}

impl Reply {
    fn new(bytes: Vec<u8>) -> Result<Self> {
        let mut reply = Self { bytes, at: 0 };
        ensure!(reply.word()? == 0x7700_0001, "wrong RPC XID");
        ensure!(reply.word()? == 1, "RPC did not reply");
        ensure!(reply.word()? == 0, "RPC call was denied");
        reply.at = 20;
        ensure!(reply.word()? == 0, "RPC call was not accepted");
        Ok(reply)
    }

    fn word(&mut self) -> Result<u32> {
        let bytes = self.bytes.get(self.at..self.at + 4).context("short XDR word")?;
        self.at += 4;
        Ok(u32::from_be_bytes(bytes.try_into()?))
    }
}

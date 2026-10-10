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

use crate::nfs_support::Rpc;
use crate::save;

const NFS3ERR_STALE: u32 = 70;
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
#[ignore = "ROUND77: active cached reads keep deleted NFS filehandles usable past T_vis"]
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

/// A same-user raw RPC read controls three reads made by an unrelated local OS user. The loopback
/// export must not let that user bypass the mounting user's private mount directory.
// contract: M1
#[test]
#[ignore = "ROUND77: any local OS user can mount and read the loopback NFS export"]
fn round77_loopback_export_is_limited_to_the_mounting_user() -> Result<()> {
    if let Ok(port) = std::env::var("ROUND77_NFS_PORT") {
        ensure!(
            rustix::process::geteuid().as_raw() == 65_534,
            "helper did not run as nobody"
        );
        let expected = std::env::var("ROUND77_NFS_BYTES")?;
        let bytes = raw_read(port.parse()?, &std::env::var("ROUND77_NFS_NAME")?)?;
        ensure!(bytes == expected.as_bytes(), "raw NFS read changed bytes");
        return Ok(());
    }
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(lab.client_dir("a").join("journal.nfs.json"))?)?;
    let port = identity["port"].as_u64().context("NFS identity has no port")? as u16;
    save(&lab.mount_path("a").join("local-user-control"), b"control")?;
    ensure!(
        raw_read(port, "local-user-control")? == b"control",
        "mounting-user control failed"
    );

    let helper_dir = tempfile::tempdir()?;
    fs::set_permissions(helper_dir.path(), fs::Permissions::from_mode(0o755))?;
    let executable = helper_dir.path().join("redteam");
    fs::copy(std::env::current_exe()?, &executable)?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
    let mut failures = 0;
    for trial in 0..3 {
        let name = format!("other-user-{trial}");
        let expected = format!("private-{trial}");
        save(&lab.mount_path("a").join(&name), expected.as_bytes())?;
        let output = Command::new("sudo")
            .args(["-n", "-u", "nobody", "env"])
            .arg(format!("ROUND77_NFS_PORT={port}"))
            .arg(format!("ROUND77_NFS_NAME={name}"))
            .arg(format!("ROUND77_NFS_BYTES={expected}"))
            .arg(&executable)
            .args([
                "--ignored",
                "--exact",
                "round77_nfs::round77_loopback_export_is_limited_to_the_mounting_user",
            ])
            .output()?;
        let read = output.status.success();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let denied = output.status.code() == Some(101) && stderr.contains("denied");
        ensure!(read || denied, "unrelated-user helper failed: {stderr}");
        eprintln!(
            "trial {trial}: unrelated user read export={read}, stderr={}",
            stderr
        );
        failures += usize::from(read);
    }
    assert_eq!(failures, 0, "unauthorized local-user reads, of 3");
    Ok(())
}

fn raw_read(port: u16, name: &str) -> Result<Vec<u8>> {
    let mut mount = Vec::new();
    opaque(&mut mount, b"/");
    let mut reply = Reply::new(call(port, MOUNT_PROGRAM, 3, 1, &mount)?)?;
    ensure!(reply.word()? == 0, "MOUNT denied");
    let root = reply.opaque()?;

    let mut lookup = Vec::new();
    opaque(&mut lookup, &root);
    opaque(&mut lookup, name.as_bytes());
    let mut reply = Reply::new(call(port, NFS_PROGRAM, 3, 3, &lookup)?)?;
    ensure!(reply.word()? == 0, "LOOKUP denied");
    let file = reply.opaque()?;

    let mut read = Vec::new();
    opaque(&mut read, &file);
    read.extend_from_slice(&0_u64.to_be_bytes());
    read.extend_from_slice(&128_u32.to_be_bytes());
    let mut reply = Reply::new(call(port, NFS_PROGRAM, 3, 6, &read)?)?;
    ensure!(reply.word()? == 0, "READ denied");
    reply.post_attr()?;
    let _count = reply.word()?;
    let _eof = reply.word()?;
    reply.opaque()
}

fn call(port: u16, program: u32, version: u32, procedure: u32, args: &[u8]) -> Result<Vec<u8>> {
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
        65_534,
        65_534,
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

    fn opaque(&mut self) -> Result<Vec<u8>> {
        let len = self.word()? as usize;
        let bytes = self
            .bytes
            .get(self.at..self.at + len)
            .context("short XDR opaque")?;
        self.at += len.next_multiple_of(4);
        Ok(bytes.to_vec())
    }

    fn post_attr(&mut self) -> Result<()> {
        if self.word()? != 0 {
            self.at += 84;
            ensure!(self.at <= self.bytes.len(), "short NFS attributes");
        }
        Ok(())
    }
}

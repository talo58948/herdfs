//! Round 89: writable export-identity files must not make NFS handles predictable.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};
use serde_json::{Value, json};

use crate::save;

const SECRET: &[u8] = b"round89 private bytes";

fn write_identity(path: &Path, export_id: u8, mode: u32) -> Result<()> {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "export_id": vec![export_id; 16],
            "port": 0,
            "creates": [],
        }))?,
    )?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn export_id(path: &Path) -> Result<Vec<u8>> {
    let state: Value = serde_json::from_slice(&fs::read(path)?)?;
    state["export_id"]
        .as_array()
        .context("NFS identity has no export id")?
        .iter()
        .map(|byte| {
            byte.as_u64()
                .and_then(|byte| u8::try_from(byte).ok())
                .context("NFS identity has an invalid export id")
        })
        .collect()
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

fn attacker(executable: &Path, port: u16, root: &[u8]) -> Result<bool> {
    let output = Command::new("sudo")
        .args(["-n", "-u", "nobody", "env"])
        .arg(format!("ROUND89_NFS_PORT={port}"))
        .arg(format!("ROUND89_NFS_ROOT={}", serde_json::to_string(root)?))
        .arg(executable)
        .args([
            "--exact",
            "round89_nfs::round89_writable_export_identity_is_replaced",
        ])
        .output()?;
    Ok(output.status.success())
}

fn attacker_read() -> Result<Vec<u8>> {
    let port = std::env::var("ROUND89_NFS_PORT")?.parse()?;
    let root: Vec<u8> = serde_json::from_str(&std::env::var("ROUND89_NFS_ROOT")?)?;

    let mut lookup = Vec::new();
    opaque(&mut lookup, &root);
    opaque(&mut lookup, b"private");
    let mut reply = Reply::new(call(port, 3, &lookup)?)?;
    ensure!(reply.word()? == 0, "attacker LOOKUP was denied");
    let file = reply.opaque()?;

    let mut read = Vec::new();
    opaque(&mut read, &file);
    read.extend_from_slice(&0_u64.to_be_bytes());
    read.extend_from_slice(&(SECRET.len() as u32).to_be_bytes());
    let mut reply = Reply::new(call(port, 6, &read)?)?;
    ensure!(reply.word()? == 0, "attacker READ was denied");
    reply.post_attr()?;
    let _count = reply.word()?;
    let _eof = reply.word()?;
    reply.opaque()
}

fn call(port: u16, procedure: u32, args: &[u8]) -> Result<Vec<u8>> {
    let mut call = Vec::new();
    for word in [
        0x8900_0001,
        0,
        2,
        100_003,
        3,
        procedure,
        1,
        20,
        0,
        0,
        0,
        0,
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
        ensure!(word(&bytes, 4)? == 1, "RPC did not reply");
        ensure!(word(&bytes, 8)? == 0, "RPC call was denied");
        ensure!(word(&bytes, 20)? == 0, "RPC call was not accepted");
        Ok(Self { bytes, at: 24 })
    }

    fn word(&mut self) -> Result<u32> {
        let value = word(&self.bytes, self.at)?;
        self.at += 4;
        Ok(value)
    }

    fn opaque(&mut self) -> Result<Vec<u8>> {
        let len = self.word()? as usize;
        let value = self
            .bytes
            .get(self.at..self.at + len)
            .context("short XDR opaque")?;
        self.at += len.next_multiple_of(4);
        Ok(value.to_vec())
    }

    fn post_attr(&mut self) -> Result<()> {
        if self.word()? != 0 {
            self.at += 84;
            ensure!(self.at <= self.bytes.len(), "short NFS attributes");
        }
        Ok(())
    }
}

fn word(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        bytes.get(at..at + 4).context("short RPC reply")?.try_into()?,
    ))
}

/// A readable identity controls three writable-only identities. Any identity exposed to another
/// local user must be replaced instead of becoming the secret prefix accepted by the NFS server.
// contract: M1, S3
#[test]
fn round89_writable_export_identity_is_replaced() -> Result<()> {
    if std::env::var_os("ROUND89_NFS_PORT").is_some() {
        ensure!(attacker_read()? == SECRET, "attacker read changed bytes");
        return Ok(());
    }
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let mut lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let path = lab.client_dir("a").join("journal.nfs.json");
    save(&lab.mount_path("a").join("private"), SECRET)?;
    let root = entry(&lab, "")?;

    let helper_dir = tempfile::tempdir()?;
    fs::set_permissions(helper_dir.path(), fs::Permissions::from_mode(0o755))?;
    let executable = helper_dir.path().join("redteam");
    fs::copy(std::env::current_exe()?, &executable)?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;

    lab.kill_client("a")?;
    write_identity(&path, 0xc0, 0o644)?;
    lab.restart_client("a")?;
    ensure!(
        export_id(&path)? != vec![0xc0; 16],
        "readable-file control was not regenerated"
    );
    let control: Value = serde_json::from_slice(&fs::read(&path)?)?;
    let control_port = control["port"].as_u64().context("control has no NFS port")? as u16;
    let mut control_root = b"OSNF".to_vec();
    control_root.extend_from_slice(&1_u32.to_be_bytes());
    control_root.extend_from_slice(&[0xc0; 16]);
    control_root.extend_from_slice(&root.id.0.to_be_bytes());
    ensure!(
        !attacker(&executable, control_port, &control_root)?,
        "regenerated control accepted the old handle"
    );

    let mut failures = 0;
    for trial in 0..3 {
        lab.kill_client("a")?;
        let chosen = 0xd0 + trial;
        write_identity(&path, chosen, 0o602)?;
        lab.restart_client("a")?;
        let state: Value = serde_json::from_slice(&fs::read(&path)?)?;
        let port = state["port"].as_u64().context("identity has no NFS port")? as u16;
        let mut chosen_root = b"OSNF".to_vec();
        chosen_root.extend_from_slice(&1_u32.to_be_bytes());
        chosen_root.extend_from_slice(&[chosen; 16]);
        chosen_root.extend_from_slice(&root.id.0.to_be_bytes());
        let preserved = export_id(&path)? == vec![chosen; 16];
        let read = attacker(&executable, port, &chosen_root)?;
        eprintln!("trial {trial}: other-writable identity preserved={preserved}, foreign-user read={read}");
        failures += usize::from(preserved && read);
    }
    assert_eq!(failures, 0, "writable identities accepted, of 3");
    Ok(())
}

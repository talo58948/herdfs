use std::fs;

use anyhow::{Context, Result, ensure};
use herdfs_lab::Profile;

use super::lab;

// contract: M1, S3
#[test]
#[ignore = "macOS: unprivileged low ports; see docs/architecture.md"]
fn unprivileged_low_port_mount_reveals_and_uses_root_handle_on_macos() -> Result<()> {
    let Some(lab) = lab(Profile::Lan)? else {
        return Ok(());
    };
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(lab.client_dir("a").join("journal.nfs.json"))?)?;
    let port = identity["port"].as_u64().context("NFS identity has no port")?;
    let script = r#"
import os, socket, struct
word = lambda value: struct.pack(">I", value)
def opaque(value): return word(len(value)) + value + bytes((-len(value)) % 4)
def receive(stream, length):
    out = b""
    while len(out) < length: out += stream.recv(length - len(out))
    return out
def call(stream, xid, program, procedure, args):
    uid = int(os.environ["MOUNT_UID"])
    values = (xid, 0, 2, program, 3, procedure, 1, 20, 0, 0, uid, uid, 0, 0, 0)
    body = b"".join(map(word, values)) + args
    stream.sendall(word(len(body) | (1 << 31)) + body)
    marker = struct.unpack(">I", receive(stream, 4))[0]
    return receive(stream, marker & ~(1 << 31))
with socket.socket() as stream:
    stream.bind(("", 700))
    stream.connect(("127.0.0.1", int(os.environ["NFS_PORT"])))
    mounted = call(stream, 1, 100005, 1, opaque(b"/"))
    assert struct.unpack_from(">I", mounted, 24)[0] == 0
    length = struct.unpack_from(">I", mounted, 28)[0]
    handle = mounted[32:32 + length]
    assert length == 32
    used = call(stream, 2, 100003, 1, opaque(handle))
    assert struct.unpack_from(">I", used, 24)[0] == 0
"#;
    let status = std::process::Command::new("/usr/bin/python3")
        .args(["-c", script])
        .env("NFS_PORT", port.to_string())
        .env("MOUNT_UID", rustix::process::geteuid().as_raw().to_string())
        .status()?;
    ensure!(status.success(), "low-port MOUNT/GETATTR probe failed");
    Ok(())
}

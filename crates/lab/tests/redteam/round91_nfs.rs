//! Round 91: NFS RENAME failures must contain both directory WCC fields.

use anyhow::{Context, Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Entry, Request, Response};
use herdfs_lab::{Lab, LabConfig, Profile, SPACE, Strategy};

use crate::nfs_support::{Rpc, opaque};

const NFS3ERR_NOENT: u32 = 2;
const NFS3ERR_BADHANDLE: u32 = 10_001;

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

fn rename(rpc: &mut Rpc, from: &[u8], to: &[u8]) -> Result<Vec<u8>> {
    let mut args = Vec::new();
    opaque(&mut args, from);
    opaque(&mut args, b"missing-source");
    opaque(&mut args, to);
    opaque(&mut args, b"destination");
    rpc.call(14, &args)
}

fn decode_rename_failure(reply: &[u8], status: u32) -> Result<()> {
    let mut reply = Reply::new(reply)?;
    ensure!(reply.u32()? == status, "RENAME returned the wrong status");
    reply.wcc()?;
    reply.wcc()?;
    Ok(())
}

struct Reply<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reply<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self> {
        ensure!(word(bytes, 4)? == 1, "RPC did not reply");
        ensure!(word(bytes, 8)? == 0, "RPC call was denied");
        ensure!(word(bytes, 20)? == 0, "RPC call was not accepted");
        Ok(Self { bytes, at: 24 })
    }

    fn u32(&mut self) -> Result<u32> {
        let value = word(self.bytes, self.at)?;
        self.at += 4;
        Ok(value)
    }

    fn skip(&mut self, len: usize) -> Result<()> {
        ensure!(self.at + len <= self.bytes.len(), "short XDR value");
        self.at += len;
        Ok(())
    }

    fn wcc(&mut self) -> Result<()> {
        if self.u32()? != 0 {
            self.skip(24)?;
        }
        if self.u32()? != 0 {
            self.skip(84)?;
        }
        Ok(())
    }
}

fn word(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        bytes.get(at..at + 4).context("short RPC reply")?.try_into()?,
    ))
}

/// A missing source with two valid handles controls three requests whose destination handle has
/// a foreign export id. Every RENAME failure must remain a complete NFSv3 `RENAME3resfail`.
// contract: M1
#[test]
#[ignore = "ROUND91: RENAME with a foreign destination handle returns a truncated failure reply"]
fn round91_rename_foreign_second_handle_has_a_complete_reply() -> Result<()> {
    if std::env::var("LAB_MOUNT").as_deref() != Ok("nfs") {
        eprintln!("skipped: set LAB_MOUNT=nfs for the NFS suite");
        return Ok(());
    }
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let root = entry(&lab, "")?;
    let mut rpc = Rpc::open(lab.client_dir("a"))?;
    let root = rpc.handle(root.id);

    decode_rename_failure(&rename(&mut rpc, &root, &root)?, NFS3ERR_NOENT)?;

    let mut failures = 0;
    for trial in 0..3 {
        let mut foreign = root.clone();
        foreign[8 + trial] ^= 1;
        let reply = rename(&mut rpc, &root, &foreign)?;
        let complete = decode_rename_failure(&reply, NFS3ERR_BADHANDLE).is_ok();
        eprintln!("trial {trial}: foreign-destination RENAME reply complete={complete}");
        failures += usize::from(!complete);
    }
    assert_eq!(failures, 0, "truncated RENAME failure replies, of 3");
    Ok(())
}

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use herdfs_core::proto::EntryId;

mod reply;

use reply::Reply;
pub use reply::denied_auth;

const NFS_PROGRAM: u32 = 100_003;
const NFS_VERSION: u32 = 3;

pub struct Rpc {
    port: u16,
    prefix: Vec<u8>,
    xid: u32,
}

pub struct WriteReply {
    pub status: u32,
    pub committed: u32,
}

impl Rpc {
    pub fn open(client: &Path) -> Result<Self> {
        let identity: serde_json::Value =
            serde_json::from_slice(&fs::read(client.join("journal.nfs.json"))?)?;
        let port = identity["port"].as_u64().context("NFS identity has no port")? as u16;
        let export = identity["export_id"]
            .as_array()
            .context("NFS identity has no export id")?;
        let mut prefix = b"OSNF".to_vec();
        prefix.extend_from_slice(&1_u32.to_be_bytes());
        prefix.extend(export.iter().map(|byte| byte.as_u64().unwrap_or_default() as u8));
        ensure!(prefix.len() == 24, "invalid persisted export id");
        Ok(Self { port, prefix, xid: 1 })
    }

    pub fn handle(&self, entry: EntryId) -> Vec<u8> {
        let mut handle = self.prefix.clone();
        handle.extend_from_slice(&entry.0.to_be_bytes());
        handle
    }

    pub fn getattr(&mut self, handle: &[u8]) -> Result<(u32, Option<u64>)> {
        let mut args = Vec::new();
        opaque(&mut args, handle);
        let reply = self.call(1, &args)?;
        let mut input = Reply::new(&reply)?;
        let status = input.u32()?;
        let size = (status == 0).then(|| input.fattr_size()).transpose()?;
        Ok((status, size))
    }

    pub fn lookup(&mut self, directory: &[u8], name: &str) -> Result<u32> {
        let mut args = Vec::new();
        opaque(&mut args, directory);
        opaque(&mut args, name.as_bytes());
        status(&self.call(3, &args)?)
    }

    pub fn read(&mut self, handle: &[u8], offset: u64, count: u32) -> Result<(u32, Vec<u8>)> {
        let mut args = Vec::new();
        opaque(&mut args, handle);
        args.extend_from_slice(&offset.to_be_bytes());
        args.extend_from_slice(&count.to_be_bytes());
        let reply = self.call(6, &args)?;
        let mut input = Reply::new(&reply)?;
        let status = input.u32()?;
        if status != 0 {
            return Ok((status, Vec::new()));
        }
        input.post_attr()?;
        let _count = input.u32()?;
        let _eof = input.u32()?;
        Ok((status, input.opaque()?))
    }

    pub fn write(&mut self, handle: &[u8], data: &[u8], stability: u32) -> Result<WriteReply> {
        let call = self.write_call(handle, data, stability);
        let reply = self.send(&call)?;
        parse_write(&reply)
    }

    pub fn commit(&mut self, handle: &[u8]) -> Result<u32> {
        let call = self.commit_call(handle);
        let reply = self.send(&call)?;
        status(&reply)
    }

    pub fn write_then_commit(&mut self, handle: &[u8], data: &[u8]) -> Result<(WriteReply, u32)> {
        let write = self.write_call(handle, data, 0);
        let commit = self.commit_call(handle);
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        send_record(&mut stream, &write)?;
        send_record(&mut stream, &commit)?;
        let write = parse_write(&read_record(&mut stream)?)?;
        let commit = status(&read_record(&mut stream)?)?;
        Ok((write, commit))
    }

    pub fn create_exclusive(
        &mut self,
        directory: &[u8],
        name: &str,
        verifier: [u8; 8],
    ) -> Result<(u32, Option<Vec<u8>>)> {
        let mut args = Vec::new();
        opaque(&mut args, directory);
        opaque(&mut args, name.as_bytes());
        args.extend_from_slice(&2_u32.to_be_bytes());
        args.extend_from_slice(&verifier);
        let reply = self.call(8, &args)?;
        let mut input = Reply::new(&reply)?;
        let status = input.u32()?;
        if status != 0 {
            return Ok((status, None));
        }
        let handle = (input.u32()? != 0).then(|| input.opaque()).transpose()?;
        Ok((status, handle))
    }

    pub fn readdir(&mut self, directory: &[u8], cookie: u64, count: u32) -> Result<Vec<(u64, String)>> {
        let mut args = Vec::new();
        opaque(&mut args, directory);
        args.extend_from_slice(&cookie.to_be_bytes());
        args.extend_from_slice(&[0; 8]);
        args.extend_from_slice(&count.to_be_bytes());
        let reply = self.call(16, &args)?;
        let mut input = Reply::new(&reply)?;
        ensure!(input.u32()? == 0, "READDIR failed");
        input.post_attr()?;
        input.skip(8)?;
        let mut entries = Vec::new();
        while input.u32()? != 0 {
            let fileid = input.u64()?;
            let name = String::from_utf8(input.opaque()?)?;
            let cookie = input.u64()?;
            ensure!(fileid == cookie, "adapter cookie differs from file id");
            entries.push((cookie, name));
        }
        let _eof = input.u32()?;
        Ok(entries)
    }

    #[allow(dead_code)]
    pub fn readdir_status(&mut self, directory: &[u8], count: u32) -> Result<u32> {
        let mut args = Vec::new();
        opaque(&mut args, directory);
        args.extend_from_slice(&0_u64.to_be_bytes());
        args.extend_from_slice(&[0; 8]);
        args.extend_from_slice(&count.to_be_bytes());
        status(&self.call(16, &args)?)
    }

    #[allow(dead_code)]
    pub fn readdir_reply(
        &mut self,
        directory: &[u8],
        count: u32,
        timeout: std::time::Duration,
    ) -> Result<Vec<u8>> {
        let mut args = Vec::new();
        opaque(&mut args, directory);
        args.extend_from_slice(&0_u64.to_be_bytes());
        args.extend_from_slice(&[0; 8]);
        args.extend_from_slice(&count.to_be_bytes());
        let mut call = rpc_header(self.next_xid(), 16, 1);
        call.extend_from_slice(&args);
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_read_timeout(Some(timeout))?;
        send_record(&mut stream, &call)?;
        read_record(&mut stream)
    }

    pub fn remove_replayed(&mut self, directory: &[u8], name: &str, reconnect: bool) -> Result<[u32; 2]> {
        let xid = self.next_xid();
        let mut call = rpc_header(xid, 12, 1);
        opaque(&mut call, directory);
        opaque(&mut call, name.as_bytes());
        if reconnect {
            return Ok([status(&self.send(&call)?)?, status(&self.send(&call)?)?]);
        }
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        send_record(&mut stream, &call)?;
        let first = status(&read_record(&mut stream)?)?;
        send_record(&mut stream, &call)?;
        let second = status(&read_record(&mut stream)?)?;
        Ok([first, second])
    }

    #[allow(dead_code)]
    pub fn remove_xid(&self, directory: &[u8], name: &str, xid: u32) -> Result<u32> {
        let mut call = rpc_header(xid, 12, 1);
        opaque(&mut call, directory);
        opaque(&mut call, name.as_bytes());
        status(&self.send(&call)?)
    }

    #[allow(dead_code)]
    pub fn create_exclusive_xid(
        &self,
        directory: &[u8],
        name: &str,
        verifier: [u8; 8],
        xid: u32,
    ) -> Result<u32> {
        let mut call = rpc_header(xid, 8, 1);
        opaque(&mut call, directory);
        opaque(&mut call, name.as_bytes());
        call.extend_from_slice(&2_u32.to_be_bytes());
        call.extend_from_slice(&verifier);
        status(&self.send(&call)?)
    }

    pub fn auth_null(&mut self) -> Result<Vec<u8>> {
        let xid = self.next_xid();
        let call = rpc_header(xid, 1, 0);
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        send_record(&mut stream, &call)?;
        read_record(&mut stream)
    }

    fn call(&mut self, procedure: u32, args: &[u8]) -> Result<Vec<u8>> {
        let xid = self.next_xid();
        let mut call = rpc_header(xid, procedure, 1);
        call.extend_from_slice(args);
        self.send(&call)
    }

    fn write_call(&mut self, handle: &[u8], data: &[u8], stability: u32) -> Vec<u8> {
        let xid = self.next_xid();
        let mut call = rpc_header(xid, 7, 1);
        opaque(&mut call, handle);
        call.extend_from_slice(&0_u64.to_be_bytes());
        call.extend_from_slice(&(data.len() as u32).to_be_bytes());
        call.extend_from_slice(&stability.to_be_bytes());
        opaque(&mut call, data);
        call
    }

    fn commit_call(&mut self, handle: &[u8]) -> Vec<u8> {
        let xid = self.next_xid();
        let mut call = rpc_header(xid, 21, 1);
        opaque(&mut call, handle);
        call.extend_from_slice(&0_u64.to_be_bytes());
        call.extend_from_slice(&0_u32.to_be_bytes());
        call
    }

    fn send(&self, call: &[u8]) -> Result<Vec<u8>> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        send_record(&mut stream, call)?;
        read_record(&mut stream)
    }

    fn next_xid(&mut self) -> u32 {
        let xid = self.xid;
        self.xid += 1;
        xid
    }
}

fn rpc_header(xid: u32, procedure: u32, auth: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for word in [xid, 0, 2, NFS_PROGRAM, NFS_VERSION, procedure, auth] {
        out.extend_from_slice(&word.to_be_bytes());
    }
    if auth == 1 {
        out.extend_from_slice(&20_u32.to_be_bytes());
        for word in [0_u32, 0, 0, 0, 0] {
            out.extend_from_slice(&word.to_be_bytes());
        }
    } else {
        out.extend_from_slice(&0_u32.to_be_bytes());
    }
    out.extend_from_slice(&0_u32.to_be_bytes());
    out.extend_from_slice(&0_u32.to_be_bytes());
    out
}

fn parse_write(reply: &[u8]) -> Result<WriteReply> {
    let mut input = Reply::new(reply)?;
    let status = input.u32()?;
    if status != 0 {
        return Ok(WriteReply { status, committed: 0 });
    }
    input.wcc()?;
    let _count = input.u32()?;
    let committed = input.u32()?;
    Ok(WriteReply { status, committed })
}

fn status(reply: &[u8]) -> Result<u32> {
    Reply::new(reply)?.u32()
}

fn opaque(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    out.resize(out.len().next_multiple_of(4), 0);
}

fn send_record(stream: &mut TcpStream, call: &[u8]) -> Result<()> {
    stream.write_all(&((call.len() as u32) | (1 << 31)).to_be_bytes())?;
    stream.write_all(call)?;
    Ok(())
}

fn read_record(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut marker = [0; 4];
    stream.read_exact(&mut marker)?;
    let len = (u32::from_be_bytes(marker) & !(1 << 31)) as usize;
    let mut reply = vec![0; len];
    stream.read_exact(&mut reply)?;
    Ok(reply)
}

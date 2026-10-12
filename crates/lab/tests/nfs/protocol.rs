use std::io::{Read, Write};
use std::net::TcpStream;

use anyhow::Result;

use super::{NFS_PROGRAM, NFS_VERSION};

pub(crate) fn rpc_header(xid: u32, procedure: u32, auth: u32) -> Vec<u8> {
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

pub(crate) fn send_record(stream: &mut TcpStream, call: &[u8]) -> Result<()> {
    stream.write_all(&((call.len() as u32) | (1 << 31)).to_be_bytes())?;
    stream.write_all(call)?;
    Ok(())
}

pub(super) fn read_record(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut marker = [0; 4];
    stream.read_exact(&mut marker)?;
    let len = (u32::from_be_bytes(marker) & !(1 << 31)) as usize;
    let mut reply = vec![0; len];
    stream.read_exact(&mut reply)?;
    Ok(reply)
}

use std::net::{Ipv4Addr, TcpStream};

use anyhow::Result;

pub(crate) fn nfs_stream(port: u16) -> Result<TcpStream> {
    Ok(TcpStream::connect((Ipv4Addr::LOCALHOST, port))?)
}

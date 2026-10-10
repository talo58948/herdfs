use std::net::{Ipv4Addr, TcpStream};

use anyhow::Result;
use rustix::net::{AddressFamily, SocketAddrV4, SocketType, bind, connect, socket};

pub(crate) fn reserved_stream(port: u16) -> Result<TcpStream> {
    let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    for source_port in (1..1024).rev() {
        let socket = socket(AddressFamily::INET, SocketType::STREAM, None)?;
        rustix::net::sockopt::set_socket_reuseaddr(&socket, true)?;
        if bind(&socket, &SocketAddrV4::new(Ipv4Addr::LOCALHOST, source_port)).is_err() {
            continue;
        }
        if connect(&socket, &target).is_ok() {
            return Ok(socket.into());
        }
    }
    anyhow::bail!("no reserved source port available; run raw NFS tests as root")
}

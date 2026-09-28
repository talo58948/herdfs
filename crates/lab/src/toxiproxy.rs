//! Toxiproxy: every client link is a proxy, shaped by a link profile and cut on demand.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::Command;

use anyhow::{Result, ensure};
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{Value, json};

use crate::process::Proc;

/// Network conditions on both of a client's links. The WAN profiles also give the store S3's
/// time to first byte, which local MinIO lacks. Toxiproxy cannot model the cost of a new
/// connection (TCP and TLS handshakes, slow start): it accepts locally and delays every chunk
/// alike, so first requests run faster here than over a real WAN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// 1 ms round trip, to a store as quick as local MinIO.
    Lan,
    /// About 90 ms round trip and 100 Mbit/s.
    Wan,
    /// About 300 ms round trip with jitter, and 1 Mbit/s.
    Bad,
    /// A measured link, such as a real WAN path that R6 compares the lab with.
    Custom {
        round_trip_ms: u32,
        kilobytes_per_sec: u32,
        store_first_byte_ms: u32,
    },
}

/// S3's median time to first byte of a GET from within its region, as R6 measured it.
const S3_FIRST_BYTE_MS: u32 = 35;

/// Which of a client's links to cut or restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Links {
    Both,
    Authority,
    Store,
}

impl Links {
    fn names(self) -> &'static [&'static str] {
        match self {
            Self::Both => &["authority", "store"],
            Self::Authority => &["authority"],
            Self::Store => &["store"],
        }
    }
}

const STREAMS: [&str; 2] = ["upstream", "downstream"];

impl Profile {
    /// The toxics of one link; the store's replies also wait for its first byte.
    fn toxics(self, store: bool) -> Vec<Value> {
        let (round_trip_ms, jitter_ms, kilobytes_per_sec, first_byte_ms) = match self {
            Self::Lan => (1, 0, None, 0),
            Self::Wan => (90, 5, Some(12_500), S3_FIRST_BYTE_MS),
            Self::Bad => (300, 50, Some(128), S3_FIRST_BYTE_MS),
            Self::Custom {
                round_trip_ms,
                kilobytes_per_sec,
                store_first_byte_ms,
            } => (round_trip_ms, 0, Some(kilobytes_per_sec), store_first_byte_ms),
        };
        let first_byte_ms = if store { first_byte_ms } else { 0 };
        // Latency delays each chunk from its arrival, so a stream's replies wait for it once.
        let one_way = [
            round_trip_ms / 2,
            round_trip_ms - round_trip_ms / 2 + first_byte_ms,
        ];
        let latency = STREAMS
            .into_iter()
            .zip(one_way)
            .filter(|(_, ms)| *ms > 0)
            .map(|(stream, ms)| toxic("latency", stream, &json!({"latency": ms, "jitter": jitter_ms})));
        let bandwidth = kilobytes_per_sec
            .into_iter()
            .flat_map(|rate| STREAMS.map(|stream| toxic("bandwidth", stream, &json!({"rate": rate}))));
        latency.chain(bandwidth).collect()
    }
}

fn toxic(kind: &str, stream: &str, attributes: &Value) -> Value {
    json!({"name": format!("{kind}-{stream}"), "type": kind, "stream": stream, "attributes": attributes})
}

pub(crate) struct Toxiproxy {
    _server: Proc,
    api: String,
    http: Client,
}

impl Toxiproxy {
    pub(crate) fn start(dir: &Path, port: u16, http: Client) -> Result<Self> {
        let mut command = Command::new("toxiproxy-server");
        command.args(["-host", "127.0.0.1", "-port", &port.to_string()]);
        let mut server = Proc::spawn("toxiproxy", command, &dir.join("toxiproxy.log"))?;
        let api = format!("http://127.0.0.1:{port}");
        server.wait_until("listening", || http.get(format!("{api}/version")).send().is_ok())?;
        Ok(Self {
            _server: server,
            api,
            http,
        })
    }

    /// Adds `client`'s links to the authority and the store, which listen on 127.0.0.1: a proxy
    /// for each on `ip`, at the same port, shaped by `profile`.
    pub(crate) fn add_links(
        &self,
        client: &str,
        ip: Ipv4Addr,
        authority_port: u16,
        store_port: u16,
        profile: Profile,
    ) -> Result<()> {
        for (link, port) in [("authority", authority_port), ("store", store_port)] {
            let name = format!("{client}-{link}");
            let listen = SocketAddr::from((ip, port));
            let upstream = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let proxy = json!({"name": name, "listen": listen, "upstream": upstream});
            send(self.http.post(format!("{}/proxies", self.api)).json(&proxy))?;
            let toxics = format!("{}/proxies/{name}/toxics", self.api);
            for toxic in profile.toxics(link == "store") {
                send(self.http.post(&toxics).json(&toxic))?;
            }
        }
        Ok(())
    }

    /// Disabling a proxy closes its connections and its listener, so connects are refused.
    pub(crate) fn set_links(&self, client: &str, links: Links, enabled: bool) -> Result<()> {
        for link in links.names() {
            let url = format!("{}/proxies/{client}-{link}", self.api);
            send(self.http.post(url).json(&json!({"enabled": enabled})))?;
        }
        Ok(())
    }
}

fn send(request: RequestBuilder) -> Result<()> {
    let reply = request.send()?;
    let status = reply.status();
    ensure!(status.is_success(), "toxiproxy: {status} {}", reply.text()?);
    Ok(())
}

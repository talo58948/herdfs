//! Toxiproxy: every client link is a proxy, shaped by a link profile and cut on demand.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::Command;

use anyhow::{Result, ensure};
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{Value, json};

use crate::process::Proc;

/// Network conditions on both of a client's links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// 1 ms round trip.
    Lan,
    /// About 90 ms round trip and 100 Mbit/s.
    Wan,
    /// About 300 ms round trip with jitter, and 1 Mbit/s.
    Bad,
    /// A measured link, such as a real WAN path that R6 compares the lab with.
    Custom {
        round_trip_ms: u32,
        kilobytes_per_sec: u32,
    },
}

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
    fn toxics(self) -> Vec<Value> {
        let (round_trip_ms, jitter_ms, kilobytes_per_sec) = match self {
            Self::Lan => (1, 0, None),
            Self::Wan => (90, 5, Some(12_500)),
            Self::Bad => (300, 50, Some(128)),
            Self::Custom {
                round_trip_ms,
                kilobytes_per_sec,
            } => (round_trip_ms, 0, Some(kilobytes_per_sec)),
        };
        let one_way = [round_trip_ms / 2, round_trip_ms - round_trip_ms / 2];
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
            for toxic in profile.toxics() {
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

    /// Makes `client`'s links swallow everything, as a real outage does: connects succeed and
    /// nothing comes back. A timeout toxic of 0 holds data until it is removed.
    pub(crate) fn silence(&self, client: &str, links: Links, silent: bool) -> Result<()> {
        let streams = links
            .names()
            .iter()
            .flat_map(|link| STREAMS.map(|stream| (link, stream)));
        for (link, stream) in streams {
            let toxics = format!("{}/proxies/{client}-{link}/toxics", self.api);
            if silent {
                let blackhole = toxic("timeout", stream, &json!({"timeout": 0}));
                send(self.http.post(&toxics).json(&blackhole))?;
            } else {
                // Absent unless the link was silenced; a 404 then is fine.
                self.http.delete(format!("{toxics}/timeout-{stream}")).send()?;
            }
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

//! The attacker: root on bob's host, with bob's token, calling the authority and the store
//! directly over bob's links.

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use herdfs_core::ChunkRef;
use herdfs_core::proto::Request;
use herdfs_lab::{Lab, SPACE};
use reqwest::StatusCode;
use reqwest::blocking::{Client as Http, RequestBuilder};
use serde_json::Value;
use tempfile::TempDir;

use crate::BOB;

/// Tries of a store GET: the loaded lab host sometimes drops a pooled connection before it
/// answers, and a GET is safe to repeat.
const GET_TRIES: usize = 3;

/// Root on bob's host, calling the authority and the store directly with bob's token. Every
/// answer is kept in `loot`, which the sweeps search.
pub(crate) struct Attacker {
    http: Http,
    authority: String,
    pub(crate) token: String,
    pub(crate) loot: TempDir,
    answers: AtomicUsize,
}

/// An HTTP answer the attacker got.
pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) body: Vec<u8>,
}

impl Reply {
    pub(crate) fn json(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

impl Attacker {
    pub(crate) fn new(lab: &Lab) -> Result<Self> {
        Ok(Self {
            http: lab.http(BOB)?,
            authority: lab.authority_url(BOB),
            token: fs::read_to_string(lab.client_dir(BOB).join("token"))?,
            loot: tempfile::Builder::new().prefix("herdfs-loot-").tempdir()?,
            answers: AtomicUsize::new(0),
        })
    }

    /// Sends `request` as JSON, so it may be one the typed protocol cannot express.
    pub(crate) fn rpc(&self, request: &Value) -> Result<Reply> {
        self.rpc_with(Some(&self.token), request)
    }

    pub(crate) fn rpc_with(&self, token: Option<&str>, request: &Value) -> Result<Reply> {
        let call = self.http.post(format!("{}/v1/rpc", self.authority)).json(request);
        self.send(authorized(call, token))
    }

    pub(crate) fn call(&self, request: &Request) -> Result<Reply> {
        self.rpc(&serde_json::to_value(request)?)
    }

    pub(crate) fn upload(&self, token: Option<&str>, chunk: &ChunkRef, bytes: &[u8]) -> Result<Reply> {
        let url = format!("{}/v1/spaces/{SPACE}/chunks/{}", self.authority, chunk.id);
        self.send(authorized(self.http.put(url).body(bytes.to_vec()), token))
    }

    /// A GET on the store, such as a presigned URL, tried again while it gets no answer.
    pub(crate) fn get(&self, url: &str) -> Result<Reply> {
        for _ in 1..GET_TRIES {
            match self.send(self.http.get(url)) {
                Err(error) if error.is::<reqwest::Error>() => continue,
                reply => return reply,
            }
        }
        self.send(self.http.get(url))
    }

    fn send(&self, request: RequestBuilder) -> Result<Reply> {
        let reply = request.send()?;
        let status = reply.status();
        let body = reply.bytes()?.to_vec();
        let number = self.answers.fetch_add(1, Ordering::Relaxed);
        fs::write(self.loot.path().join(number.to_string()), &body)?;
        Ok(Reply { status, body })
    }
}

fn authorized(request: RequestBuilder, token: Option<&str>) -> RequestBuilder {
    match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    }
}

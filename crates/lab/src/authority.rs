//! The authority process, and calls straight to it.

use std::fmt::Display;
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use herdfs_core::proto::{self, ErrorCode, Request, Response};
use reqwest::blocking::Client as Http;

use crate::issuer::{self, Issuer};
use crate::process::{self, Proc};
use crate::store::{self, S3_HOST};

pub(crate) struct Authority {
    process: Option<Proc>,
    pub(crate) port: u16,
    http: Http,
}

impl Authority {
    /// An authority that is not running yet, to listen on 127.0.0.1:`port`.
    pub(crate) fn new(port: u16, http: Http) -> Self {
        Self {
            process: None,
            port,
            http,
        }
    }

    pub(crate) fn kill(&mut self) {
        self.process = None;
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(Proc::id)
    }

    /// Starts the authority on its database in `dir`, after killing a running one, and waits
    /// until `/v1/health` answers.
    pub(crate) fn restart(&mut self, dir: &Path, store_port: u16, issuer: &Issuer) -> Result<()> {
        self.kill();
        let listen = format!("127.0.0.1:{}", self.port);
        let mut command = Command::new(process::binary(process::AUTHORITY)?);
        command
            .args(["--listen", &listen, "--db"])
            .arg(dir.join("authority.db"))
            .args(["--s3-endpoint", &format!("http://127.0.0.1:{store_port}")])
            .args(["--s3-public-endpoint", &format!("http://{S3_HOST}:{store_port}")])
            .args(["--s3-bucket", store::BUCKET, "--s3-region", store::REGION])
            .args(["--oidc-issuer", issuer.url(), "--oidc-audience", issuer::AUDIENCE])
            .envs(store::STORE_CREDENTIALS);
        let mut authority = Proc::spawn("authority", command, &dir.join("authority.log"))?;
        let health = format!("http://{listen}/v1/health");
        authority.wait_until("healthy", || {
            let reply = self.http.get(&health).send();
            reply.is_ok_and(|reply| reply.status().is_success())
        })?;
        self.process = Some(authority);
        Ok(())
    }

    pub(crate) fn rpc(&self, token: &str, request: &Request) -> Result<Response, proto::Error> {
        let url = format!("http://127.0.0.1:{}/v1/rpc", self.port);
        let call = self.http.post(url).bearer_auth(token).json(request);
        let reply = call.send().map_err(unavailable)?;
        let status = reply.status();
        let body = reply.bytes().map_err(unavailable)?;
        if status.is_success() {
            return serde_json::from_slice(&body).map_err(unavailable);
        }
        let text = String::from_utf8_lossy(&body);
        Err(serde_json::from_slice(&body).unwrap_or_else(|_| unavailable(format!("{status}: {text}"))))
    }
}

/// A transport failure, or a reply outside the protocol, as a protocol error.
fn unavailable(error: impl Display) -> proto::Error {
    proto::Error::new(ErrorCode::Unavailable, error.to_string())
}

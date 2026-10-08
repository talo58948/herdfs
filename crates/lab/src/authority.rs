//! The authority process, and calls straight to it.

use std::fmt::Display;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use herdfs_core::proto::{self, ErrorCode, Request, Response};
use reqwest::blocking::Client as Http;

use crate::issuer::{self, Issuer};
use crate::process::{self, Proc};
use crate::store::{self, S3_HOST};

/// Retention and collection settings for the authority, in place of its production defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Collection {
    /// How long a presigned chunk URL stays valid.
    pub grant_ttl: Duration,
    /// How often the collector runs.
    pub every: Duration,
    /// How old an unreferenced chunk object must be before the collector deletes it.
    pub grace: Duration,
    /// The newest versions of each entry kept, its head included.
    pub retain_versions: u32,
    /// Versions of each entry kept by age.
    pub retain_days: u32,
}

impl Collection {
    /// Short enough that a test sees a collection within seconds, with the grace at least twice
    /// the grant TTL, as the authority requires.
    pub const QUICK: Self = Self {
        grant_ttl: Duration::from_secs(2),
        every: Duration::from_secs(1),
        grace: Duration::from_secs(5),
        retain_versions: 3,
        retain_days: 0,
    };

    fn args(self) -> Vec<String> {
        let seconds = |duration: Duration| duration.as_secs().to_string();
        [
            ("--grant-ttl", seconds(self.grant_ttl)),
            ("--gc-every", seconds(self.every)),
            ("--gc-grace", seconds(self.grace)),
            ("--retain-versions", self.retain_versions.to_string()),
            ("--retain-days", self.retain_days.to_string()),
        ]
        .into_iter()
        .flat_map(|(flag, value)| [flag.to_owned(), value])
        .collect()
    }
}

pub(crate) struct Authority {
    process: Option<Proc>,
    pub(crate) port: u16,
    http: Http,
    /// Production defaults when `None`.
    collection: Option<Collection>,
}

impl Authority {
    /// An authority that is not running yet, to listen on 127.0.0.1:`port`.
    pub(crate) fn new(port: u16, http: Http, collection: Option<Collection>) -> Self {
        Self {
            process: None,
            port,
            http,
            collection,
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
    pub(crate) fn restart(
        &mut self,
        dir: &Path,
        store_port: u16,
        issuer: &Issuer,
        local_issuer: bool,
    ) -> Result<()> {
        self.kill();
        let listen = format!("127.0.0.1:{}", self.port);
        let mut command = Command::new(process::binary(process::AUTHORITY)?);
        command
            .args(["--listen", &listen, "--db"])
            .arg(dir.join("authority.db"))
            .args(["--s3-endpoint", &format!("http://127.0.0.1:{store_port}")])
            .args(["--s3-public-endpoint", &format!("http://{S3_HOST}:{store_port}")])
            .args(["--s3-bucket", store::BUCKET, "--s3-region", store::REGION])
            .args(self.collection.map(Collection::args).unwrap_or_default())
            .envs(store::STORE_CREDENTIALS);
        if local_issuer {
            command.arg("--local-issuer");
        } else {
            command.args(["--oidc-issuer", issuer.url(), "--oidc-audience", issuer::AUDIENCE]);
        }
        let mut authority = Proc::spawn("authority", command, &dir.join("authority.log"))?;
        let health = format!("http://{listen}/v1/health");
        authority.wait_until("healthy", || {
            let reply = self.http.get(&health).send();
            reply.is_ok_and(|reply| reply.status().is_success())
        })?;
        self.process = Some(authority);
        Ok(())
    }

    pub(crate) fn issue_token(&self, dir: &Path, user: &str) -> Result<String> {
        let output = process::run(
            Command::new(process::binary(process::AUTHORITY)?)
                .args(["issue-token", "--db"])
                .arg(dir.join("authority.db"))
                .args(["--user", user]),
        )?;
        Ok(output.trim().to_owned())
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

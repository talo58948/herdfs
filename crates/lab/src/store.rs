//! The object store: a MinIO server holding the `herdfs` bucket.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use reqwest::Url;
use reqwest::blocking::Client;
use rusty_s3::actions::ListObjectsV2;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

use crate::process::Proc;

/// The host presigned URLs name. Each client resolves it to its own store link.
pub(crate) const S3_HOST: &str = "s3.lab";
pub(crate) const BUCKET: &str = "herdfs";
pub(crate) const REGION: &str = "us-east-1";
const ACCESS_KEY: &str = "lab-access";
const SECRET_KEY: &str = "lab-secret";
/// The store's root credentials, as the authority reads them from its environment. Only the
/// authority may hold them (contract S1).
pub const STORE_CREDENTIALS: [(&str, &str); 2] = [
    ("AWS_ACCESS_KEY_ID", ACCESS_KEY),
    ("AWS_SECRET_ACCESS_KEY", SECRET_KEY),
];
const PRESIGN_TTL: Duration = Duration::from_secs(300);
/// MinIO's data directory, under the lab's.
const DATA: &str = "minio";

pub(crate) struct Store {
    _server: Proc,
    bucket: Bucket,
    credentials: Credentials,
    /// Reaches the store directly, not through any client's link.
    http: Client,
    /// Where MinIO keeps object bytes on disk.
    pub(crate) data: PathBuf,
}

impl Store {
    /// Starts MinIO on 127.0.0.1:`port` with its data under `dir`, and creates the bucket.
    /// `http` must resolve `s3.lab` to 127.0.0.1.
    pub(crate) fn start(dir: &Path, port: u16, http: &Client) -> Result<Self> {
        let mut command = Command::new("minio");
        command
            .args(["server", "--quiet", "--address", &format!("127.0.0.1:{port}")])
            .arg("--certs-dir")
            .arg(dir.join("minio-certs"))
            .arg(dir.join(DATA))
            .env("MINIO_ROOT_USER", ACCESS_KEY)
            .env("MINIO_ROOT_PASSWORD", SECRET_KEY)
            .env("MINIO_BROWSER", "off");
        let mut server = Proc::spawn("minio", command, &dir.join("minio.log"))?;
        let endpoint = Url::parse(&format!("http://{S3_HOST}:{port}"))?;
        let bucket = Bucket::new(endpoint, UrlStyle::Path, BUCKET, REGION)?;
        let credentials = Credentials::new(ACCESS_KEY, SECRET_KEY);
        let create = bucket.create_bucket(&credentials).sign(PRESIGN_TTL);
        // MinIO reports itself ready before it accepts requests, so creating the bucket is the probe.
        server.wait_until("ready", || {
            let reply = http.put(create.clone()).send();
            reply.is_ok_and(|reply| reply.status().is_success())
        })?;
        Ok(Self {
            _server: server,
            bucket,
            credentials,
            http: http.clone(),
            data: dir.join(DATA),
        })
    }

    /// Every object whose key starts with `prefix`, with its size in bytes.
    pub(crate) fn list(&self, prefix: &str) -> Result<BTreeMap<String, u64>> {
        let mut objects = BTreeMap::new();
        let mut next: Option<String> = None;
        loop {
            let url = {
                let mut list = self.bucket.list_objects_v2(Some(&self.credentials));
                list.with_prefix(prefix);
                if let Some(token) = &next {
                    list.with_continuation_token(token.as_str());
                }
                list.sign(PRESIGN_TTL)
            };
            let page = self.http.get(url).send()?.error_for_status()?.text()?;
            let page = ListObjectsV2::parse_response(&page)?;
            objects.extend(page.contents.into_iter().map(|object| (object.key, object.size)));
            let Some(token) = page.next_continuation_token else {
                return Ok(objects);
            };
            next = Some(token);
        }
    }

    pub(crate) fn presign_get(&self, key: &str) -> Url {
        self.bucket
            .get_object(Some(&self.credentials), key)
            .sign(PRESIGN_TTL)
    }

    pub(crate) fn presign_put(&self, key: &str) -> Url {
        self.bucket
            .put_object(Some(&self.credentials), key)
            .sign(PRESIGN_TTL)
    }
}

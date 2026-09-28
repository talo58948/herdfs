//! Calls to the lab's services that bypass the mounts: the authority's API, tokens, and the store.

use std::net::SocketAddr;

use anyhow::Result;
use herdfs_core::proto::{self, Request, Response};
use reqwest::Url;
use reqwest::blocking::Client as Http;

use crate::store::S3_HOST;
use crate::{BadToken, Lab};

impl Lab {
    /// Calls the authority directly as `user`, with the groups of the first client that mounts
    /// as `user`, if any.
    pub fn rpc(&self, user: &str, request: &Request) -> Result<Response, proto::Error> {
        let client = self.clients.iter().find(|client| client.spec.user == user);
        let groups = client
            .map(|client| client.spec.groups.as_slice())
            .unwrap_or_default();
        self.rpc_with(&self.issuer.token(user, groups), request)
    }

    /// Calls the authority directly with any token, such as a [`BadToken`].
    pub fn rpc_with(&self, token: &str, request: &Request) -> Result<Response, proto::Error> {
        self.authority.rpc(token, request)
    }

    /// A token from the lab's issuer for `user` in `groups`.
    pub fn token(&self, user: &str, groups: &[&str]) -> String {
        self.issuer.token(user, groups)
    }

    pub fn bad_token(&self, user: &str, kind: BadToken) -> String {
        self.issuer.bad_token(user, kind)
    }

    pub fn issuer_url(&self) -> &str {
        self.issuer.url()
    }

    /// An HTTP client as on client `name`'s host: `s3.lab` resolves to its store link.
    pub fn http(&self, name: &str) -> Result<Http> {
        let link = SocketAddr::from((self.client(name).ip, self.store_port));
        Ok(Http::builder().resolve(S3_HOST, link).build()?)
    }

    /// A presigned GET for `key` in the bucket, naming host `s3.lab` as the authority's grants do.
    pub fn presign_get(&self, key: &str) -> Url {
        self.store.presign_get(key)
    }

    /// A presigned PUT for `key` in the bucket, naming host `s3.lab`.
    pub fn presign_put(&self, key: &str) -> Url {
        self.store.presign_put(key)
    }
}

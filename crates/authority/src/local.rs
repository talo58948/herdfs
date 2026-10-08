//! The persisted issuer and revocable machine tokens for local mode.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use herdfs_core::proto::{self, ErrorCode};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Authority, Fail};

const AUDIENCE: &str = "herdfs";
const KEY_ID: &str = "local";
const PKCS8_PREFIX: &[u8] = &[
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// A signing key persisted beside an authority database.
pub struct LocalIssuer {
    stored: Stored,
    key: SigningKey,
}

#[derive(Deserialize, Serialize)]
struct Stored {
    issuer: String,
    secret: String,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    aud: &'static str,
    sub: &'a str,
    iat: u64,
    exp: u64,
    jti: &'a str,
}

impl LocalIssuer {
    /// Loads the local issuer, creating it for `listen` when it does not exist.
    pub fn open(db: &Path, listen: SocketAddr) -> anyhow::Result<Self> {
        let path = key_path(db)?;
        let issuer = issuer_url(listen);
        let stored = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => create(&path, issuer.clone())?,
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        anyhow::ensure!(
            stored.issuer == issuer,
            "local issuer is {}, but --listen implies {issuer}",
            stored.issuer
        );
        Self::from_stored(stored)
    }

    /// Loads an issuer previously created by a running local authority.
    pub fn load(db: &Path) -> anyhow::Result<Self> {
        let path = key_path(db)?;
        let bytes = fs::read(&path).with_context(|| {
            format!(
                "reading {}; start the authority once with --local-issuer first",
                path.display()
            )
        })?;
        Self::from_stored(serde_json::from_slice(&bytes)?)
    }

    /// Issues one revocable token for `user` that lasts for `ttl`.
    pub fn issue(&self, authority: &Authority, user: &str, ttl: Duration) -> anyhow::Result<(String, String)> {
        anyhow::ensure!(!user.is_empty(), "--user must not be empty");
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let exp = now.checked_add(ttl.as_secs()).context("token expiry is too large")?;
        let id = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>());
        authority.db.execute(
            "INSERT INTO local_tokens (id, user, expires_ms) VALUES (?1, ?2, ?3)",
            params![id, user, i64::try_from(exp.saturating_mul(1000))?],
        )?;
        let claims = Claims {
            iss: &self.stored.issuer,
            aud: AUDIENCE,
            sub: user,
            iat: now,
            exp,
            jti: &id,
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(KEY_ID.into());
        let token = jsonwebtoken::encode(&header, &claims, &self.encoding_key())?;
        Ok((id, token))
    }

    /// Revokes an issued token immediately.
    pub fn revoke(&self, authority: &Authority, id: &str) -> anyhow::Result<()> {
        let changed = authority
            .db
            .execute("UPDATE local_tokens SET revoked = 1 WHERE id = ?1", [id])?;
        anyhow::ensure!(changed == 1, "unknown token id {id:?}");
        Ok(())
    }

    pub(crate) fn issuer(&self) -> &str {
        &self.stored.issuer
    }

    pub(crate) fn audience(&self) -> &'static str {
        AUDIENCE
    }

    pub(crate) fn discovery(&self) -> Value {
        json!({
            "issuer": self.stored.issuer,
            "jwks_uri": format!("{}/.well-known/jwks.json", self.stored.issuer),
            "id_token_signing_alg_values_supported": ["EdDSA"]
        })
    }

    pub(crate) fn jwks(&self) -> Value {
        let public = URL_SAFE_NO_PAD.encode(self.key.verifying_key().as_bytes());
        json!({"keys": [{
            "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig",
            "kid": KEY_ID, "x": public
        }]})
    }

    fn from_stored(stored: Stored) -> anyhow::Result<Self> {
        let secret: [u8; 32] = URL_SAFE_NO_PAD
            .decode(&stored.secret)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("local issuer key is not 32 bytes"))?;
        Ok(Self {
            stored,
            key: SigningKey::from_bytes(&secret),
        })
    }

    fn encoding_key(&self) -> EncodingKey {
        let mut der = Vec::from(PKCS8_PREFIX);
        der.extend_from_slice(self.key.as_bytes());
        EncodingKey::from_ed_der(&der)
    }
}

impl Authority {
    pub(crate) fn check_local_token(&self, id: &str, user: &str, now_ms: i64) -> Result<(), proto::Error> {
        let active: bool = self
            .db
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM local_tokens
                 WHERE id = ?1 AND user = ?2 AND revoked = 0 AND expires_ms > ?3)",
                params![id, user, now_ms],
                |row| row.get(0),
            )
            .map_err(Fail::from)?;
        if active {
            return Ok(());
        }
        Err(proto::Error::new(ErrorCode::Unauthenticated, "local token is expired or revoked"))
    }
}

fn create(path: &Path, issuer: String) -> anyhow::Result<Stored> {
    let stored = Stored {
        issuer,
        secret: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
    };
    let mut file = private_file(path)?;
    serde_json::to_writer(&mut file, &stored)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(stored)
}

fn private_file(path: &Path) -> anyhow::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))
}

fn key_path(db: &Path) -> anyhow::Result<PathBuf> {
    let name = db.file_name().context("database path has no file name")?;
    let mut key = name.to_os_string();
    key.push(".local-issuer.json");
    Ok(db.with_file_name(key))
}

fn issuer_url(listen: SocketAddr) -> String {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    format!("http://{}", SocketAddr::new(ip, listen.port()))
}

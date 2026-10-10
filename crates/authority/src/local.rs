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
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header};
use herdfs_core::proto;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::{Authority, Fail};

const AUDIENCE: &str = "herdfs";
pub(crate) const KEY_ID: &str = "local";
// RFC 8410 PKCS#8 header for a 32-byte Ed25519 seed.
const PKCS8_PREFIX: &[u8] = &[
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// A signing key persisted beside an authority database.
pub struct LocalIssuer {
    stored: Stored,
    key: SigningKey,
}

/// A newly registered local token and the id used to revoke it.
pub struct Issued {
    pub id: String,
    pub token: String,
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
            Ok(bytes) => {
                serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => create(&path, issuer.clone())?,
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        anyhow::ensure!(
            stored.issuer == issuer,
            "local issuer is {}, but --listen implies {issuer}; changing --listen requires deleting the key file {} and reissuing every token",
            stored.issuer,
            path.display()
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

    fn token(&self, user: &str, id: &str, now: u64, exp: u64) -> anyhow::Result<String> {
        let claims = Claims {
            iss: &self.stored.issuer,
            aud: AUDIENCE,
            sub: user,
            iat: now,
            exp,
            jti: id,
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(KEY_ID.into());
        Ok(jsonwebtoken::encode(&header, &claims, &self.encoding_key())?)
    }

    pub(crate) fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key.verifying_key().as_bytes())
    }

    pub(crate) fn decoding_key(&self) -> DecodingKey {
        DecodingKey::from_ed_der(self.key.verifying_key().as_bytes())
    }

    pub(crate) fn issuer(&self) -> &str {
        &self.stored.issuer
    }

    pub(crate) fn audience(&self) -> &'static str {
        AUDIENCE
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
    /// Issues and registers one revocable token for `user` that lasts for `ttl`.
    pub fn issue_local_token(
        &mut self,
        issuer: &LocalIssuer,
        user: &str,
        ttl: Duration,
    ) -> anyhow::Result<Issued> {
        anyhow::ensure!(!user.is_empty(), "--user must not be empty");
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let exp = now
            .checked_add(ttl.as_secs())
            .context("token expiry is too large")?;
        let id = format!("{:032x}", rand::random::<u128>());
        let token = issuer.token(user, &id, now, exp)?;
        let now_ms = i64::try_from(now.saturating_mul(1000))?;
        let exp_ms = i64::try_from(exp.saturating_mul(1000))?;
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM local_tokens WHERE expires_ms <= ?1", [now_ms])?;
        tx.execute(
            "INSERT INTO local_tokens (id, user, expires_ms) VALUES (?1, ?2, ?3)",
            params![id, user, exp_ms],
        )?;
        tx.commit()?;
        Ok(Issued { id, token })
    }

    /// Revokes an issued token immediately.
    pub fn revoke_local_token(&self, id: &str) -> anyhow::Result<()> {
        let changed = self
            .db
            .execute("UPDATE local_tokens SET revoked = 1 WHERE id = ?1", [id])?;
        anyhow::ensure!(changed == 1, "unknown token id {id:?}");
        Ok(())
    }

    pub(crate) fn check_local_token(&self, id: &str, user: &str, now_ms: i64) -> Result<bool, proto::Error> {
        let active = self
            .db
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM local_tokens
                 WHERE id = ?1 AND user = ?2 AND revoked = 0 AND expires_ms > ?3)",
                params![id, user, now_ms],
                |row| row.get(0),
            )
            .map_err(Fail::from)?;
        Ok(active)
    }
}

fn create(path: &Path, issuer: String) -> anyhow::Result<Stored> {
    let stored = Stored {
        issuer,
        secret: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
    };
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(format!(".tmp-{:016x}", rand::random::<u64>()));
    let temporary = PathBuf::from(temporary);
    let result = (|| {
        let mut file = private_file(&temporary)?;
        serde_json::to_writer(&mut file, &stored)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
            .with_context(|| format!("renaming {} to {}", temporary.display(), path.display()))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
        Ok(stored)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
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

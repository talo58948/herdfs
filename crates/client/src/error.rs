//! Client errors and the errno each one surfaces as.

use std::io;

use fuser::Errno;
use herdfs_core::ChunkId;
use herdfs_core::proto::{self, ErrorCode};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The authority understood the request and refused it.
    #[error("authority: {0}")]
    Api(#[from] proto::Error),
    #[error("network: {0}")]
    Network(#[from] reqwest::Error),
    #[error("chunk {0} does not match its id")]
    Corrupt(ChunkId),
    /// An answer that is not the protocol's, such as a proxy's error page.
    #[error("unexpected answer from the authority: {0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The authority counts as unreachable after a recent call failed to reach it.
    #[error("the authority is unreachable")]
    Offline,
    /// A local condition with a fixed errno.
    #[error("{0:?}")]
    Errno(Errno),
}

impl From<Errno> for Error {
    fn from(errno: Errno) -> Self {
        Self::Errno(errno)
    }
}

impl Error {
    pub fn errno(&self) -> Errno {
        match self {
            Self::Api(error) => match error.code {
                ErrorCode::NotFound => Errno::ENOENT,
                ErrorCode::Exists => Errno::EEXIST,
                ErrorCode::NotEmpty => Errno::ENOTEMPTY,
                ErrorCode::NotDir => Errno::ENOTDIR,
                ErrorCode::IsDir => Errno::EISDIR,
                ErrorCode::Denied | ErrorCode::Unauthenticated => Errno::EACCES,
                ErrorCode::Invalid => Errno::EINVAL,
                ErrorCode::Conflict => Errno::EBUSY,
                ErrorCode::Unavailable => Errno::EIO,
            },
            Self::Io(error) => error.raw_os_error().map_or(Errno::EIO, Errno::from_i32),
            Self::Errno(errno) => *errno,
            Self::Network(_) | Self::Offline | Self::Corrupt(_) | Self::Protocol(_) => Errno::EIO,
        }
    }

    /// True when the authority could not be reached, rather than refusing.
    pub fn is_offline(&self) -> bool {
        matches!(self, Self::Network(_) | Self::Offline)
            || matches!(self, Self::Api(error) if error.code == ErrorCode::Unavailable)
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Api(error) if error.code == ErrorCode::NotFound)
    }

    /// True when the authority refused for good, so retrying cannot help.
    pub fn is_final(&self) -> bool {
        matches!(self, Self::Api(error)
            if !matches!(error.code, ErrorCode::Unavailable | ErrorCode::Unauthenticated))
    }
}

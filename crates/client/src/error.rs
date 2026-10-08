//! Client errors and the errno each one surfaces as.

use std::io;

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
    /// The authority's answer did not start within the deadline of its call.
    #[error("the authority did not answer in time")]
    TimedOut,
    /// A local condition with a fixed errno.
    #[error("{0:?}")]
    Errno(i32),
}

impl From<i32> for Error {
    fn from(errno: i32) -> Self {
        Self::Errno(errno)
    }
}

impl Error {
    pub fn errno(&self) -> i32 {
        match self {
            Self::Api(error) => match error.code {
                ErrorCode::NotFound => libc::ENOENT,
                ErrorCode::Exists => libc::EEXIST,
                ErrorCode::NotEmpty => libc::ENOTEMPTY,
                ErrorCode::NotDir => libc::ENOTDIR,
                ErrorCode::IsDir => libc::EISDIR,
                ErrorCode::Denied | ErrorCode::Unauthenticated => libc::EACCES,
                ErrorCode::Invalid => libc::EINVAL,
                ErrorCode::Conflict => libc::EBUSY,
                ErrorCode::Unavailable => libc::EIO,
            },
            Self::Io(error) => error.raw_os_error().unwrap_or(libc::EIO),
            Self::Errno(errno) => *errno,
            Self::Network(_) | Self::Offline | Self::TimedOut | Self::Corrupt(_) | Self::Protocol(_) => {
                libc::EIO
            }
        }
    }

    /// True when the authority could not be reached, rather than refusing.
    pub fn is_offline(&self) -> bool {
        matches!(self, Self::Network(_) | Self::Offline | Self::TimedOut)
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

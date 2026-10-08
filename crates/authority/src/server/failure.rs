use axum::{Json, http::StatusCode, response::IntoResponse};
use herdfs_core::proto::{self, ErrorCode};

/// A protocol error as an HTTP response.
pub(super) struct Failure(proto::Error);

impl From<proto::Error> for Failure {
    fn from(error: proto::Error) -> Self {
        Self(error)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> axum::response::Response {
        let status = match self.0.code {
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Exists
            | ErrorCode::NotEmpty
            | ErrorCode::NotDir
            | ErrorCode::IsDir
            | ErrorCode::Conflict => StatusCode::CONFLICT,
            ErrorCode::Denied => StatusCode::FORBIDDEN,
            ErrorCode::Unauthenticated => StatusCode::UNAUTHORIZED,
            ErrorCode::Invalid => StatusCode::BAD_REQUEST,
            ErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, Json(self.0)).into_response()
    }
}

pub(super) fn invalid(message: impl Into<String>) -> proto::Error {
    proto::Error::new(ErrorCode::Invalid, message)
}

pub(super) fn unauthenticated(message: impl Into<String>) -> proto::Error {
    proto::Error::new(ErrorCode::Unauthenticated, message)
}

pub(super) fn unavailable(error: impl std::fmt::Display) -> proto::Error {
    tracing::warn!(%error, "unavailable");
    proto::Error::new(ErrorCode::Unavailable, "service unavailable")
}

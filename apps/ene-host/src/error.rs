use connectrpc::{ConnectError, ErrorCode};
use std::fmt::{self, Display, Formatter};

#[derive(Debug)]
pub(crate) struct ApiError(pub ErrorCode, pub &'static str);

impl Display for ApiError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.1)
    }
}

impl std::error::Error for ApiError {}

pub(crate) fn rpc(error: anyhow::Error) -> ConnectError {
    if let Some(ApiError(code, message)) = error.downcast_ref() {
        return ConnectError::new(*code, *message);
    }
    tracing::warn!("host operation failed");
    ConnectError::internal("host operation failed")
}

pub(crate) fn invalid(message: &'static str) -> anyhow::Error {
    ApiError(ErrorCode::InvalidArgument, message).into()
}

pub(crate) fn conflict(message: &'static str) -> anyhow::Error {
    ApiError(ErrorCode::Aborted, message).into()
}

pub(crate) fn missing(message: &'static str) -> anyhow::Error {
    ApiError(ErrorCode::NotFound, message).into()
}

pub(crate) fn denied(message: &'static str) -> anyhow::Error {
    ApiError(ErrorCode::PermissionDenied, message).into()
}

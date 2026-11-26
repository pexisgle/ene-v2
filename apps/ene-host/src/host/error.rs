//! The host's own failure type. Connect status mapping lives at the RPC edge,
//! so the host stays independent of any transport.
pub(crate) type Result<T> = std::result::Result<T, HostError>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum HostError {
    /// The request is malformed or carries an unsupported value.
    #[error("{0}")]
    Invalid(String),
    /// The addressed entity does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The request contradicts current state, such as a stale revision.
    #[error("{0}")]
    Conflict(String),
    /// The addressed entity already exists.
    #[error("{0}")]
    AlreadyExists(String),
    /// The caller is not allowed to perform the operation.
    #[error("{0}")]
    Permission(String),
    /// A bounded resource, such as the command queue, is exhausted.
    #[error("{0}")]
    Exhausted(String),
    /// The capability exists but is not available right now.
    #[error("{0}")]
    Unavailable(String),
    /// The host is correctly configured, but this operation needs setup first.
    #[error("{0}")]
    FailedPrecondition(String),
    /// The state database or the session log failed. The host stops.
    #[error("{0}")]
    Storage(String),
    /// A defect the host cannot classify.
    #[error("{0}")]
    Internal(String),
}

/// The classified reason an operation failed.
///
/// One place decides how a failure is named on the wire and whether repeating
/// it can help, so the wire mapping and the retry decision cannot drift apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum ErrorCode {
    InvalidArgument,
    NotFound,
    Aborted,
    AlreadyExists,
    PermissionDenied,
    ResourceExhausted,
    Unavailable,
    FailedPrecondition,
    #[strum(serialize = "storage_error")]
    Storage,
    Internal,
}

impl HostError {
    /// The classified reason, which drives both the wire code and retryability.
    pub(crate) fn code(&self) -> ErrorCode {
        match self {
            Self::Invalid(_) => ErrorCode::InvalidArgument,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::Conflict(_) => ErrorCode::Aborted,
            Self::AlreadyExists(_) => ErrorCode::AlreadyExists,
            Self::Permission(_) => ErrorCode::PermissionDenied,
            Self::Exhausted(_) => ErrorCode::ResourceExhausted,
            Self::Unavailable(_) => ErrorCode::Unavailable,
            Self::FailedPrecondition(_) => ErrorCode::FailedPrecondition,
            Self::Storage(_) => ErrorCode::Storage,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }

    /// Whether this failure means the host can no longer trust its state and
    /// must stop accepting new work.
    pub(crate) fn is_storage(&self) -> bool {
        matches!(self, Self::Storage(_))
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Invalid(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::AlreadyExists(message)
            | Self::Permission(message)
            | Self::Exhausted(message)
            | Self::Unavailable(message)
            | Self::FailedPrecondition(message)
            | Self::Storage(message)
            | Self::Internal(message) => message,
        }
    }
}

pub(crate) use ene_domain::Failure;

impl From<ene_domain::DomainError> for HostError {
    fn from(error: ene_domain::DomainError) -> Self {
        Self::Invalid(error.to_string())
    }
}

impl From<ene_store::StoreError> for HostError {
    fn from(error: ene_store::StoreError) -> Self {
        match error {
            ene_store::StoreError::Invalid(message) => Self::Invalid(message),
            ene_store::StoreError::NotFound(message) => Self::NotFound(message),
            ene_store::StoreError::Conflict(message) => Self::Conflict(message),
            ene_store::StoreError::Permission(message) => Self::Permission(message),
            ene_store::StoreError::Unavailable(message) => Self::Unavailable(message),
            ene_store::StoreError::Storage(message) => Self::Storage(message),
        }
    }
}

impl From<ene_voice::VoiceSessionError> for HostError {
    fn from(error: ene_voice::VoiceSessionError) -> Self {
        match error {
            ene_voice::VoiceSessionError::NoSubscriber
            | ene_voice::VoiceSessionError::StreamMissing => {
                Self::FailedPrecondition(error.to_string())
            }
            ene_voice::VoiceSessionError::AudioQueueFull
            | ene_voice::VoiceSessionError::StreamClosed => Self::Unavailable(error.to_string()),
            ene_voice::VoiceSessionError::NoOwner => Self::NotFound(error.to_string()),
        }
    }
}

impl From<ene_work::WorkError> for HostError {
    fn from(error: ene_work::WorkError) -> Self {
        match error {
            ene_work::WorkError::Invalid(message) => Self::Invalid(message),
            ene_work::WorkError::Conflict(message) => Self::Conflict(message),
            ene_work::WorkError::Unavailable(message) => Self::Unavailable(message),
            ene_work::WorkError::Internal(message) => Self::Internal(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_keeps_its_wire_code() {
        let errors = [
            HostError::Invalid(String::new()),
            HostError::NotFound(String::new()),
            HostError::Conflict(String::new()),
            HostError::AlreadyExists(String::new()),
            HostError::Permission(String::new()),
            HostError::Exhausted(String::new()),
            HostError::Unavailable(String::new()),
            HostError::FailedPrecondition(String::new()),
            HostError::Storage(String::new()),
            HostError::Internal(String::new()),
        ];
        let codes: [&str; 10] = errors.map(|error| error.code().into());
        assert_eq!(
            codes,
            [
                "invalid_argument",
                "not_found",
                "aborted",
                "already_exists",
                "permission_denied",
                "resource_exhausted",
                "unavailable",
                "failed_precondition",
                "storage_error",
                "internal",
            ]
        );
    }
}

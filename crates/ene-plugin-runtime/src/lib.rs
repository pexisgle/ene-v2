pub mod limits;
pub mod package;
pub mod transport;
pub mod worker;
pub use package::Package;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid_package")]
    Invalid,
    #[error("package_changed")]
    Changed,
    #[error("unsupported_capability")]
    Unsupported,
    #[error("permission_required")]
    Denied,
    #[error("worker_unavailable")]
    Unavailable,
    #[error("invalid_worker_protocol")]
    Protocol,
    #[error("cancelled")]
    Cancelled,
    #[error("worker_deadline_exceeded")]
    Timeout,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

pub mod contract;

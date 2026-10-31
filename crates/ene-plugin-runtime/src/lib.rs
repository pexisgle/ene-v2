pub mod instance;
pub mod limits;
pub mod package;
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
    #[error("plugin_unavailable")]
    Unavailable,
    #[error("plugin_execution_failed: {0:#}")]
    Execution(#[source] extism::Error),
    #[error("host_service_failed: {0}")]
    Service(#[from] ene_plugin::Failure),
    #[error("invalid_plugin_contract")]
    Protocol,
    #[error("cancelled")]
    Cancelled,
    #[error("plugin_deadline_exceeded")]
    Timeout,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

pub mod contract;

use ene_api_types::host::ApiErrorBody;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid API endpoint or Bearer token")]
    InvalidConnection,
    #[error("invalid idempotency key")]
    InvalidKey,
    #[error("request transport failed")]
    Transport,
    #[error("authentication required")]
    Unauthorized,
    #[error("{0}")]
    Api(ApiFailure),
    #[error("invalid or oversized API response")]
    Protocol,
}
#[derive(Debug)]
pub struct ApiFailure {
    pub status: u16,
    pub body: ApiErrorBody,
}
impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.body.error.message, self.body.error.code)
    }
}

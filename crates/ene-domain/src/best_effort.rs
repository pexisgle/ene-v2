/// Reports a failure that leaves state wrong.
///
/// Use where the caller cannot propagate but a person might need to know.
pub fn warn_if_err<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) {
    if let Err(error) = result {
        tracing::warn!(%error, context, "an operation failed");
    }
}

/// Reports a failure that is expected and harmless.
///
/// Cleanup and shutdown paths use this so a routine failure stays out of the
/// warning log.
pub fn debug_if_err<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) {
    if let Err(error) = result {
        tracing::debug!(%error, context, "a best-effort step failed");
    }
}

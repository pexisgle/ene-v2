//! Stating what happens to a result nobody acts on.
//!
//! Three grades, so a teardown failure does not become log noise and a real
//! failure does not vanish:
//!
//! - [`warn_if_err`] — the failure leaves state wrong (a save, an RPC, a UI
//!   update). Logged at `warn` because someone may need to intervene.
//! - [`debug_if_err`] — cleanup and best-effort teardown (a temp file, a
//!   socket at shutdown). Logged at `debug` because it is expected to fail
//!   sometimes and never needs action.
//! - [`ignore_teardown`] — a Win32 `BOOL`/handle result from destroying an
//!   object that is about to stop existing either way.

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

/// Discards a result on purpose, where nothing can be done about it.
///
/// The Win32 destroy calls return a `BOOL` that only reports whether the
/// object existed; the object is gone either way, so there is no failure to
/// report and no state left wrong.
#[inline]
pub fn ignore_teardown<T>(_result: T) {
    // Intentionally empty: the call's effect is the point, not its return.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The helpers must be callable without binding the result, which is what
    /// lets them replace a bare discard.
    #[test]
    fn helpers_accept_both_outcomes() {
        warn_if_err(Ok::<(), &str>(()), "no-op");
        warn_if_err(Err::<(), _>("failed"), "warned");
        debug_if_err(Ok::<(), &str>(()), "no-op");
        debug_if_err(Err::<(), _>("failed"), "debugged");
        ignore_teardown(1);
    }
}

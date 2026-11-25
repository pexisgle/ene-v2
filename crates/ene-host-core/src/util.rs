//! Pure host-wide utility helpers: timestamps, digests, ID validation and bounded text.
use jiff::Timestamp;
use sha2::{Digest, Sha256};

/// The current time in UTC, RFC 3339.
pub fn now() -> String {
    // The stored columns are compared as text, so every stamp keeps the same
    // second precision the schedule rows are written with.
    format!("{:.0}", Timestamp::now())
}

/// The SHA-256 of `bytes`, used for action payloads and idempotency hashes.
pub fn digest(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

/// Opaque ids are 1..=128 printable ASCII characters. Anything else is a
/// client mistake, not a lookup miss.
pub fn validate_id(id: &str) -> Result<(), ene_domain::DomainError> {
    ene_domain::validate_id(id)
}

/// Truncates `value` to at most `limit` characters.
pub fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// Reports a cleanup step that failed, without changing the caller's shape.
///
/// Rollback and teardown paths cannot do much about a failure, but dropping it
/// silently is how a host ends up in a state nobody can explain. Callers stay
/// a single expression, so this does not add a branch to their complexity.
pub fn log_if_err<T, E: std::fmt::Display>(result: std::result::Result<T, E>, context: &str) {
    if let Err(error) = result {
        tracing::warn!(%error, context, "a cleanup step failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_counts_characters_not_bytes() {
        assert_eq!(truncate("あいうえお", 3), "あいう");
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abc", 0), "");
    }

    /// The stored columns are compared as text, so a stamp that grew a
    /// fractional part would stop matching the rows written before it.
    #[test]
    fn a_stamp_keeps_whole_second_precision() {
        let stamp = now();
        assert!(stamp.ends_with('Z'), "{stamp:?}");
        assert!(!stamp.contains('.'), "{stamp:?}");
    }

    #[test]
    fn digest_is_a_sha256_over_the_exact_bytes() {
        let hashed = digest(b"abc");
        assert_eq!(hashed.len(), 32);
        assert_eq!(hashed, digest(b"abc"), "the same bytes must hash the same");
        assert_ne!(hashed, digest(b"abd"));
    }

    #[test]
    fn only_printable_ascii_ids_are_accepted() {
        assert!(validate_id("client-1").is_ok());
        for rejected in ["", "has space", "tab\there", "émoji"] {
            let outcome = validate_id(rejected);
            assert!(outcome.is_err(), "{rejected:?} must be rejected");
        }
        assert!(validate_id(&"a".repeat(128)).is_ok());
        assert!(validate_id(&"a".repeat(129)).is_err());
    }
}

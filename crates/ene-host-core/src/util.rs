//! Pure host-wide utility helpers: timestamps, digests, and bounded text.
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

/// Truncates `value` to at most `limit` characters.
pub fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
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
}

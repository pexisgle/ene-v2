//! Pure host-wide utility helpers: timestamps, digests, ID validation and bounded text.
use super::error::Result;
use jiff::Timestamp;
use sha2::{Digest, Sha256};

/// The current time in UTC, RFC 3339.
pub(crate) fn now() -> String {
    // The stored columns are compared as text, so every stamp keeps the same
    // second precision the schedule rows are written with.
    format!("{:.0}", Timestamp::now())
}

/// The SHA-256 of `bytes`, used for action payloads and idempotency hashes.
pub(crate) fn digest(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

/// Opaque ids are 1..=128 printable ASCII characters. Anything else is a
/// client mistake, not a lookup miss.
pub(crate) fn validate_id(id: &str) -> Result<()> {
    ene_domain::validate_id(id).map_err(Into::into)
}

/// Truncates `value` to at most `limit` characters.
pub(crate) fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

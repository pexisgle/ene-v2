//! Pure host-wide utility helpers: timestamps, digests, ID validation and bounded text.
use super::error::{HostError, Result};
use jiff::Timestamp;
use sha2::{Digest, Sha256};

/// The current time in UTC, RFC 3339.
pub(crate) fn now() -> String {
    // The stored columns are compared as text, so every stamp keeps the same
    // second precision the schedule rows are written with.
    format!("{:.0}", Timestamp::now())
}

/// Parses RFC 3339 into seconds since the Unix epoch.
pub(crate) fn epoch(text: &str) -> Option<i64> {
    text.parse::<Timestamp>().ok().map(Timestamp::as_second)
}

/// The SHA-256 of `bytes`, used for action payloads and idempotency hashes.
pub(crate) fn digest(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

/// Opaque ids are 1..=128 printable ASCII characters. Anything else is a
/// client mistake, not a lookup miss.
pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.chars().count() > 128
        || id.bytes().any(|byte| !(0x21..=0x7e).contains(&byte))
    {
        return Err(HostError::Invalid(
            ("id must be 1-128 printable ASCII characters").into(),
        ));
    }
    Ok(())
}

/// The caller's stamp, or `fallback` (one `now()` shared by a row) when the
/// caller did not set one.
pub(crate) fn or_stamp(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

/// Truncates `value` to at most `limit` characters.
pub(crate) fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

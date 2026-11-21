//! Embedding space identifiers and pending backfill row descriptors.
use sha2::{Digest, Sha256};

/// The preprocessing version in the composite space id. Bump it whenever the
/// text fed to the model changes, so old vectors cannot be compared.
pub const PREP_VERSION: &str = "v1";

/// A vector row waiting for an embedding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingEmbedding {
    pub id: String,
    pub content: String,
    pub content_revision: u64,
}

/// The composite key that identifies an embedding space: the model, a short
/// non-secret fingerprint of `(adapter, endpoint, options, model)`, its
/// dimensions, and the text preprocessing version.
pub fn space_id(
    adapter: &str,
    endpoint: &str,
    options_raw: &str,
    model: &str,
    dimensions: usize,
) -> String {
    let parsed_options: serde_json::Value =
        serde_json::from_str::<serde_json::Value>(options_raw.trim())
            .ok()
            .filter(|value| value.is_object())
            .unwrap_or_else(|| serde_json::json!({}));
    let canonical_value = serde_json::json!({
        "adapter": adapter.trim(),
        "endpoint": endpoint.trim_end_matches('/'),
        "model": model.trim(),
        "options": parsed_options,
    });
    let canonical = serde_json_canonicalizer::to_string(&canonical_value)
        .unwrap_or_else(|_| canonical_value.to_string());
    let digest = Sha256::digest(canonical.as_bytes());
    let mut hash = String::with_capacity(16);
    for byte in digest.into_iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(hash, "{byte:02x}");
    }
    format!("{}@{hash}:{dimensions}:{PREP_VERSION}", model.trim())
}

/// Reads the dimensions back from a composite space id. A space of another
/// preprocessing version is not comparable and therefore not this space.
pub fn dimensions_of_space(space: &str) -> Option<usize> {
    let (rest, version) = space.rsplit_once(':')?;
    if version != PREP_VERSION {
        return None;
    }
    let (_, dimensions) = rest.rsplit_once(':')?;
    dimensions.parse().ok()
}

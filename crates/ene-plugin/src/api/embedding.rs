//! Embedding contract.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Texts to embed with one model call.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingRequest {
    /// Provider endpoint from the connection; the host overwrites it.
    pub endpoint: String,
    /// Adapter options from the connection; the host overwrites them.
    pub options: Value,
    /// Embedding model to call.
    pub model: String,
    /// The texts to embed, in the order the reply must follow.
    pub inputs: Vec<String>,
    /// The width the host asks for. Zero keeps the provider's own.
    pub dimensions: usize,
}

/// The vectors a provider returned.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingResult {
    /// One vector per input, in the same order and of the same length.
    pub vectors: Vec<Vec<f32>>,
}

/// What an embedding provider supports beyond the base contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingCapabilities {
    /// Whether one request may carry several inputs.
    pub batch: bool,
}

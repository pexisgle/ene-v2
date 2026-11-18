//! Embedding contract.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingRequest {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    /// The texts to embed, in the order the reply must follow.
    pub inputs: Vec<String>,
    /// The width the host asks for. Zero keeps the provider's own.
    pub dimensions: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingResult {
    /// One vector per input, in the same order and of the same length.
    pub vectors: Vec<Vec<f32>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingCapabilities {
    /// Whether one request may carry several inputs.
    pub batch: bool,
}

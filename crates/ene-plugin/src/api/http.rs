//! Host-brokered HTTP payloads.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One HTTP exchange the broker performs for the plugin.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpRequest {
    /// HTTP method, upper case.
    pub method: String,
    /// Absolute URL; the broker refuses anything off the connection origin.
    pub url: String,
    /// Extra request headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Request body, base64 encoded.
    #[serde(default)]
    pub body_base64: Option<String>,
    /// Ask the broker to hand chunks out as the network delivers them.
    #[serde(default)]
    pub stream: bool,
}

/// One chunk of a brokered response.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpChunk {
    /// Handle the rest of the response is read through.
    pub handle: String,
    /// HTTP status, repeated on every chunk.
    pub status: u16,
    /// Response headers, sent with the first chunk only.
    pub headers: BTreeMap<String, String>,
    /// Body bytes, base64 encoded.
    pub data_base64: String,
    /// Whether this is the last chunk.
    pub done: bool,
}

//! Connect RPC messages and services generated from `proto/host.proto`.
//! Consumers use these types directly; transport limits belong to Connect RPC.
//! Enable `client` for loopback HTTP/Bearer transport setup with `connect`.

/// Generated v1 contract, available in every build.
// Suppresses style and size lints inside generated `_connectrpc.rs`; remove when codegen complies.
#[allow(
    missing_debug_implementations,
    clippy::all,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::large_futures
)]
pub mod rpc {
    include!(concat!(env!("OUT_DIR"), "/_connectrpc.rs"));
    pub use ene::host::v1::*;
}

/// Built-in tools owned by `ene-host`. Client capabilities and plugins may not shadow them.
pub use ene_policy::BUILTIN_TOOLS;
/// Desktop GUI capability executed by a work agent on its bound client.
pub use ene_policy::COMPUTER_USE;
/// How much damage an approval's action could do. The desktop renders it on
/// the approval card and the host decides it, so it travels with the contract.
pub use ene_policy::RiskLevel;
pub use rpc::*;
/// Maximum width or height of a reported JPEG frame.
pub const MAX_PERCEPTION_FRAME_EDGE: u32 = 1280;
/// Maximum encoded JPEG size before Connect serialization.
pub const MAX_PERCEPTION_JPEG_BYTES: usize = 512 * 1024;

/// Structured payload for `Companion.body` (`schema = "ene.body.v1"`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BodyBinding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[f32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_percent: Option<f32>,
}

/// Structured payload for `Companion.voice` (`schema = "ene.voice.v1"`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VoiceBinding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

#[cfg(all(feature = "client", not(target_family = "wasm")))]
mod client;
#[cfg(all(feature = "client", not(target_family = "wasm")))]
pub use client::connect;
#[cfg(all(feature = "client-tls", not(target_family = "wasm")))]
pub use client::{connect_remote, validate_remote_endpoint};

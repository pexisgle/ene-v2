//! Connect RPC messages and services generated from `proto/host.proto`.
//! Consumers use these types directly; transport limits belong to Connect RPC.
//! Enable `client` for loopback HTTP/Bearer transport setup with `connect`.

/// Generated v1 contract, available in every build.
pub mod rpc {
    #[cfg(not(target_family = "wasm"))]
    connectrpc::include_generated!();
    #[cfg(target_family = "wasm")]
    include!(concat!(env!("OUT_DIR"), "/_connectrpc.rs"));
    pub use ene::host::v1::*;
}

pub use rpc::*;

impl From<serde_json::Value> for JsonDocument {
    fn from(value: serde_json::Value) -> Self {
        Self {
            json: value.to_string(),
            ..Default::default()
        }
    }
}
impl From<&serde_json::Value> for JsonDocument {
    fn from(value: &serde_json::Value) -> Self {
        Self::from(value.clone())
    }
}

#[cfg(all(feature = "client", not(target_family = "wasm")))]
mod client;
#[cfg(all(feature = "client", not(target_family = "wasm")))]
pub use client::connect;

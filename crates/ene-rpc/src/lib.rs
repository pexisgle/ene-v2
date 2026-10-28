//! Connect RPC messages and services generated from `proto/host.proto`.
//! Consumers use these types directly; transport limits belong to Connect RPC.
//! Enable `client` for native TLS/Bearer transport setup with `connect`.

/// Generated v1 contract, available in every build.
pub mod rpc {
    connectrpc::include_generated!();
    pub use ene::host::v1::*;
}

pub use rpc::*;

#[cfg(all(feature = "client", not(target_family = "wasm")))]
mod client;
#[cfg(all(feature = "client", not(target_family = "wasm")))]
pub use client::connect;

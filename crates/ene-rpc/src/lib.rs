//! Connect RPC messages and services generated from `proto/host.proto`.
//! Consumers use these types directly; transport limits belong to Connect RPC.

/// Generated v1 contract, available in every build.
pub mod rpc {
    #[cfg(not(target_family = "wasm"))]
    connectrpc::include_generated!();
    #[cfg(target_family = "wasm")]
    include!(concat!(env!("OUT_DIR"), "/_connectrpc.rs"));
    pub use ene::host::v1::*;
}

pub use rpc::*;

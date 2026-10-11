//! Connect RPC messages and services generated from `proto/host.proto`.
//! Consumers use these types directly; transport limits belong to Connect RPC.

/// Generated v1 contract, available in every build.
pub mod rpc {
    connectrpc::include_generated!();
    pub use ene::host::v1::*;
}

pub use rpc::*;

//! Shared storage/plugin DTOs, notification rules and generated Connect RPC contract; no UI, database or transport.
pub mod budget;
pub mod host;
pub mod plugin;
pub mod provider;
pub mod sync;
pub use host::*;

/// Generated v1 contract. The handwritten host/provider types above remain
/// the storage and plugin boundary, not the source of the RPC contract.
#[cfg(feature = "rpc")]
pub mod rpc {
    connectrpc::include_generated!();
    pub use ene::host::v1::*;
}

#[cfg(feature = "rpc")]
pub mod rpc_conversion;

#[cfg(feature = "rpc")]
mod plugin_rpc_conversion;

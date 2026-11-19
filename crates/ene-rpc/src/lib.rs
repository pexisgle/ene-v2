//! Connect RPC messages and services generated from `proto/host.proto`.
//! Consumers use these types directly; transport limits belong to Connect RPC.
//! Enable `client` for loopback HTTP/Bearer transport setup with `connect`.

/// Generated v1 contract, available in every build.
pub mod rpc {
    include!(concat!(env!("OUT_DIR"), "/_connectrpc.rs"));
    pub use ene::host::v1::*;
}

pub use rpc::*;
/// Desktop GUI capability executed by a work agent on its bound client.
pub const COMPUTER_USE: &str = "computer.use";
/// Maximum width or height of a reported JPEG frame.
pub const MAX_PERCEPTION_FRAME_EDGE: u32 = 1280;
/// Maximum encoded JPEG size before Connect serialization.
pub const MAX_PERCEPTION_JPEG_BYTES: usize = 512 * 1024;
mod speech;
pub use speech::plain_reply;

#[cfg(all(feature = "client", not(target_family = "wasm")))]
mod client;
#[cfg(all(feature = "client", not(target_family = "wasm")))]
pub use client::connect;

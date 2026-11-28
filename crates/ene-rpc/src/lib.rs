//! The shared Connect contract generated from `host.proto`.

pub mod wire {
    include!(concat!(env!("OUT_DIR"), "/_connectrpc.rs"));
    pub use ene::host::v2::*;
}
pub use wire::*;

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
pub use client::connect;
#[cfg(feature = "client-tls")]
pub use client::{connect_remote, validate_remote_endpoint};

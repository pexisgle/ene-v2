//! Versioned client, renderer and provider communication contracts.
pub mod avatar;
pub mod host;
#[cfg(feature = "ipc")]
pub mod ipc;
pub mod provider;
pub mod sync;

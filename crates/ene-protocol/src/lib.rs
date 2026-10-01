//! Versioned client, renderer and provider communication contracts.
pub mod avatar;
#[cfg(feature = "ipc")]
pub mod ipc;
pub mod provider;
pub mod host;

//! Internal IPC contracts and Remoc.
#[cfg(feature = "host-plugin-contracts")]
pub mod plugin;
#[cfg(feature = "provider-contracts")]
pub mod provider;
#[cfg(feature = "renderer")]
pub mod renderer;
#[cfg(feature = "transport")]
pub use remoc;

//! Internal IPC contracts and Remoc.
#[cfg(feature = "provider-plugin")]
pub mod provider;
#[cfg(feature = "renderer")]
pub mod renderer;
#[cfg(feature = "transport")]
pub use remoc;

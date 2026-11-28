//! Bounded Extism execution for the host and desktop.
//!
//! Snapshot package code before execution, disable WASI and direct HTTP, and
//! enforce the same operation, service, resource and cancellation contracts
//! in both processes.
mod contract;
pub mod instance;
pub mod limits;
pub mod package;

pub use package::Package;
type Result<T> = anyhow::Result<T>;

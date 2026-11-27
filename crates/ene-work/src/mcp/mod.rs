//! MCP transport helpers (`npx` direct resolution, stdio/HTTP transport opening, and naming).

pub mod npx;
pub mod transport;

pub use npx::is_npx_command;
pub use transport::{ConnectFailure, backoff_delay, exposed_name, open};

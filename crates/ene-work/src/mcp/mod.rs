//! MCP transport helpers (`npx` direct resolution, stdio/HTTP transport opening, and naming).

pub mod npx;
pub mod transport;

pub use npx::is_npx_command;
pub use transport::{
    CheckState, ConnectFailure, Desired, Launch, backoff_delay, exposed_name,
    is_config_error_message, open,
};

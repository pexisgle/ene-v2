//! Work session helpers: harness update buffering, run stall tracking, and MCP transport.

pub mod harness;
pub mod mcp;
pub mod tracker;

pub use harness::{
    HarnessBuffer, UpdateEffects, arguments_of, prompt_of, select_permission_outcome, stop_outcome,
};
pub use mcp::{CheckState, ConnectFailure, Desired, Launch, is_npx_command};
pub use tracker::{RunTracker, STALL_TIMEOUT, new_run};

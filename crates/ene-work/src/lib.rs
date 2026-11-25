//! Work session helpers: ACP stdio JSON-RPC bridge, harness update buffering,
//! run stall tracking, and MCP stdio/HTTP transport helpers.

// Enforced here first: this crate is free of the lint while the app crates still
// have sites. Add this attribute to a crate only once `cargo clippy -p <crate>`
// reports zero for it. The workspace cannot enable it centrally yet, because a
// crate cannot override `[lints] workspace = true`.
#![warn(clippy::needless_pass_by_value)]
pub mod acp;
pub mod error;
pub mod harness;
pub mod mcp;
pub mod tracker;

pub use acp::{AcpBridge, AcpLaunchSpec, PermissionResponder, spawn_acp_process};
pub use error::{Result, WorkError};
pub use harness::{
    HarnessBuffer, UpdateEffects, arguments_of, prompt_of, select_permission_outcome, stop_outcome,
};
pub use mcp::{CheckState, ConnectFailure, Desired, Launch, is_npx_command};
pub use tracker::{RunTracker, STALL_TIMEOUT, WorkReceipts, new_run};

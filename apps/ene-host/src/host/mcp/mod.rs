//! MCP servers as ordinary tools (tech-stack §B).
//!
//! One enabled `Mcp` connection is one server. The host connects on its
//! runtime, lists the server's tools and registers each allowed one like any
//! other tool, so the agent does not care whether a tool is built in, served by
//! a client or served by an MCP process.
//!
//! The tools belong to the operator's own configuration rather than to a
//! client's declaration, so they run without an approval card: the gate passes
//! them the same way it passes the built-in host tools, and stays in charge of
//! what a *client* may ask the host to do.
//!
//! A server that is disabled, deleted, re-configured or disconnected loses its
//! tools: the connection closes and the tool entities despawn.
mod failures;
mod projection;
mod settings;
mod supervisor;

pub(crate) use ene_work::mcp::{CheckState, is_npx_command};
#[cfg(test)]
use projection::tool_names;
pub(crate) use projection::tool_servers;
pub(crate) use supervisor::{McpSupervisor, check_state, request_reconnect};

use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;

pub(crate) fn register(app: &mut App) {
    app.init_resource::<McpSupervisor>()
        .add_systems(Update, supervisor::sync.after(AgentLoop));
}

/// The plugin the host adds once.
#[derive(Default)]
pub(crate) struct McpPlugin;

impl Plugin for McpPlugin {
    fn build(&self, app: &mut App) {
        register(app);
    }
}

#[cfg(test)]
mod tests;

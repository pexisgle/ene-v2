//! MCP servers as ordinary tools (tech-stack §B).
//!
//! One enabled `Mcp` connection is one server. The host connects on its
//! runtime, lists the server's tools and registers each allowed one like any
//! other tool, so the agent does not care whether a tool is built in, served by
//! a client or served by an MCP process.
//!
//! MCP tools are offered to work agents. The gate uses the owning server to
//! evaluate the saved policy and requests approval when required.
//!
//! A server that is disabled, deleted, re-configured or disconnected loses its
//! tools: the connection closes and the tool entities despawn.
mod failures;
mod projection;
mod registration;
mod settings;
mod supervisor;

pub(crate) use ene_work::mcp::is_npx_command;
#[cfg(test)]
use projection::tool_names;
pub(crate) use registration::McpRegistration;
pub(crate) use settings::ConnectionStamp;
pub(crate) use supervisor::{CheckState, Mcp};

use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;

pub(crate) fn register(app: &mut App) {
    app.init_resource::<Mcp>()
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

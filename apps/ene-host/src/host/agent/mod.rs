//! Agents: the head companions and the work agents, their models, prompts,
//! stream notifications and turn lifecycle.
//!
//! The kernel owns turns and conversations; this module binds an agent to a
//! companion (or a work run), keeps its model connection current and projects
//! the conversation into the host's notification stream.
//!
//! The pieces are split by responsibility: [`ene_host_core::prompt`] is the static persona,
//! [`spawn`] owns one agent per companion and per open run, [`binding`] keeps
//! its model current, [`transcript`] and [`turns`] publish what happens to it.
mod binding;
mod spawn;
mod transcript;
mod turns;

use super::model::ModelFactory;
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;

pub(crate) use super::resources::{CompanionAgent, WorkAgent};
use binding::Connections;
pub(crate) use binding::{clear_model_cache, deliver, ensure_model};
#[cfg(test)]
pub(crate) use spawn::companion_agent_id;
pub(crate) use spawn::{
    WorkReports, agent_of, reconcile, spawn_companion, spawn_work_agent, sync_companion,
    work_agent_id,
};
pub(crate) use transcript::{entry_text, role_of};

// `tests.rs` reaches the split pieces through `use super::*`, so these names
// stay bound here exactly as they were before the split.
#[cfg(test)]
use super::{model::Credentials, notify::Notifier};
#[cfg(test)]
use ene_rpc as w;
#[cfg(test)]
use rig_core::completion::Message as CompletionMessage;
#[cfg(test)]
use rig_core::conversation::Author;
#[cfg(test)]
use rig_core::message::UserContent;
#[cfg(test)]
use rig_ecs::model::Connection as ModelConnection;

use transcript::{publish_messages, stream_deltas};
use turns::{log_tools, turn_settled, turn_started};

/// The companion an agent belongs to, whichever role it serves.
pub(super) fn companion_of<'a>(
    companion: Option<&'a CompanionAgent>,
    work: Option<&'a WorkAgent>,
) -> Option<&'a str> {
    companion
        .map(|agent| agent.companion_id.as_str())
        .or_else(|| work.map(|agent| agent.companion_id.as_str()))
}

/// Answers device tool calls whose client never responded.
fn expire_devices(world: &mut World) {
    let now = std::time::Instant::now();
    let expired: Vec<String> = {
        let waiting = world.resource::<super::resources::Waiting>();
        waiting
            .by_operation
            .iter()
            .filter(|(_, waiter)| waiter.deadline <= now)
            .map(|(operation, _)| operation.clone())
            .collect()
    };
    for operation in expired {
        super::client::device::expire(world, &operation);
    }
}

/// The host's agent plugin: projection and device timeouts.
pub(crate) struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Connections>()
            .init_resource::<ModelFactory>()
            .init_resource::<WorkReports>()
            .add_observer(
                |event: On<Add<Settled>>, commands: Commands, params: turns::TurnSettledParams| {
                    turn_settled(event.entity, commands, params);
                },
            )
            .add_systems(
                Update,
                (
                    publish_messages.after(AgentLoop),
                    turn_started.after(AgentLoop),
                    log_tools.after(AgentLoop),
                    expire_devices.after(AgentLoop),
                ),
            )
            .add_systems(AgentPass, stream_deltas.in_set(ReviewSystems::Edit));
    }
}

#[cfg(test)]
mod tests;

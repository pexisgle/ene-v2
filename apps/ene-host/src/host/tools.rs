//! Shared open-tool dispatch and the one built-in tool.
use super::{error::HostError, fail};
use bevy_ecs::prelude::*;
use rig_core::tool::ToolResult;
use rig_derive::rig_tool;
use rig_ecs::prelude::*;

/// Get the current Unix time in milliseconds.
#[rig_tool]
fn current_time() -> std::result::Result<String, std::convert::Infallible> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
        .to_string())
}

pub(crate) fn current_time_tool() -> impl rig_core::tool::Tool {
    CurrentTime
}

pub(crate) fn run_tool<A: Clone + Send + Sync + 'static>(
    handler: fn(&mut World, Entity, &str, A) -> std::result::Result<String, HostError>,
) -> impl Fn(On<ToolCalled<A>>, Commands) {
    move |called: On<ToolCalled<A>>, mut commands: Commands| {
        let ToolCalled {
            call,
            agent,
            tool_call,
            args,
            ..
        } = called.event().clone();
        let call_id = tool_call.id.to_string();
        commands.queue(move |world: &mut World| {
            let result = handler(world, agent, &call_id, args);
            answer(world, call, result);
        });
    }
}

pub(crate) fn answer(
    world: &mut World,
    call: Entity,
    result: std::result::Result<String, HostError>,
) {
    let outcome = match result {
        Ok(text) => Outcome::from(ToolResult::success(text.into())),
        Err(error) => {
            if error.is_storage() {
                fail(world, error.to_string());
            }
            Outcome::refused(error.to_string())
        }
    };
    if let Some(mut decision) = world.get_mut::<Decision>(call) {
        decision.decide(Decider::plugin("ene-host"), outcome);
    }
}

#[allow(dead_code)]
fn _uses(_: &HostError) {}

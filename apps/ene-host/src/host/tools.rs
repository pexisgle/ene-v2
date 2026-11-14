//! The one dispatch path for host-bound open tools: run a handler against the
//! world and answer the effect with a tool result or a refusal.
use super::error::HostError;
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;

/// Wraps a synchronous world handler into an open-tool observer.
pub(crate) fn run_tool<A: Clone + Send + Sync + 'static>(
    handler: fn(&mut World, Entity, &str, A) -> std::result::Result<String, HostError>,
) -> impl Fn(On<ToolCalled<A>>, Commands) {
    move |called: On<ToolCalled<A>>, mut commands: Commands| {
        let event = called.event();
        let (call, agent, tool_call, args) = (
            event.call,
            event.agent,
            event.tool_call.clone(),
            event.args.clone(),
        );
        let _ = tool_call;
        commands.queue(move |world: &mut World| {
            let call_id = world
                .get::<ToolRequest>(call)
                .map(|request| request.call.id.to_string())
                .unwrap_or_default();
            let result = handler(world, agent, &call_id, args);
            match result {
                Ok(text) => super::permission::answer(world, call, &text),
                Err(error) => {
                    if error.is_storage() {
                        super::fail(world, error.to_string());
                    }
                    super::permission::refuse(world, call, &error.to_string());
                }
            }
        });
    }
}

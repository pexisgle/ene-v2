//! Memory control and the read side used by the mind tools.
use super::{mind_view, notify_mind};
use crate::host::{db::Result, db::Store, error::HostError, memory::MemoryAction};
use bevy_ecs::prelude::*;
use ene_rpc as w;

pub(crate) fn control_memory(
    world: &mut World,
    request: w::MemoryControlRequest,
) -> Result<w::MemoryControlResult> {
    let action = MemoryAction::parse(&request.action)
        .ok_or_else(|| HostError::invalid("unknown memory action"))?;
    let store = world.resource::<Store>().clone();
    let memory = store.control_memory(&request.companion_id, &request.memory_id, action)?;
    let view = mind_view(&store, &request.companion_id)?;
    notify_mind(world, &request.companion_id, &view);
    Ok(w::MemoryControlResult {
        memory_id: memory.id,
        importance: u32::from(memory.importance),
        audiences: memory.audiences,
        revision: memory.memory_revision,
        ..Default::default()
    })
}

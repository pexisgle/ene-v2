//! The companion: the author's profile, the mind it grew and its model
//! assignments. Query functions live in `db::companions`.

use super::error::{HostError, Result};
use ene_mind::JsonDoc;
pub(crate) use ene_mind::{
    Companion, ModelAssignment, ModelAssignments, Profile, Role, State, default_ene_companion,
};

// === Host operations ======================================================

pub(crate) mod consolidate;
pub(crate) mod embedding;
pub(crate) mod plugin;
pub(crate) mod recall;
pub(crate) mod tools;

use super::{agent, db::Store, notify::Notifier, wire};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use ene_store::util::now;
use rig_ecs::prelude::*;

/// Registers the parent's memory tools and the dynamic-context systems.
pub(crate) struct MindPlugin;

impl Plugin for MindPlugin {
    fn build(&self, app: &mut App) {
        tools::register(app);
        app.add_plugins(recall::RecallPlugin);
        // The vector cache and its HTTP source.
        embedding::register(app);
    }
}

pub(crate) fn list(store: &Store) -> Result<Vec<w::Companion>> {
    Ok(store
        .companion_repo()
        .companions()?
        .iter()
        .map(wire::companion)
        .collect())
}

pub(crate) fn get(store: &Store, id: &str) -> Result<w::Companion> {
    store
        .companion_repo()
        .companion(id)?
        .map(|companion| wire::companion(&companion))
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))
}

fn document_field(field: Option<&w::JsonDocument>) -> JsonDoc {
    field.map_or_else(JsonDoc::empty, wire::document_from)
}

pub(crate) fn create(
    world: &mut World,
    request: w::CreateCompanionRequest,
) -> Result<w::Companion> {
    let store = world.resource::<Store>().clone();
    if store.companion_repo().companions()?.len() >= 16 {
        return Err(HostError::Exhausted(("too many companions").into()));
    }
    let profile = request
        .profile
        .as_option()
        .map_or_default(wire::profile_from);
    let assignments = request.assignments.into_option().map_or_default(Into::into);
    let companion = Companion {
        id: uuid::Uuid::new_v4().to_string(),
        name: request.name.clone(),
        mind: request.mind.clone(),
        profile,
        profile_revision: 1,
        state: State::default(),
        state_revision: 1,
        body: document_field(request.body.as_option()),
        voice: document_field(request.voice.as_option()),
        assignments,
        persona: request.persona.clone(),
        voice_identifier: request.voice_identifier.clone(),
        revision: 1,
        created_at: now(),
        updated_at: now(),
    };
    companion.validate()?;
    store.companion_repo().insert_companion(&companion)?;
    agent::spawn_companion(world, &companion);
    agent::sync_companion(world, &companion.id)?;
    {
        let notifier = world.resource_mut::<Notifier>();
        notify(&notifier, &companion);
    }
    Ok(wire::companion(&companion))
}

fn set_assignment(target: &mut Option<ModelAssignment>, patch: Option<&w::AssignmentPatch>) {
    if let Some(change) = patch.and_then(|patch| patch.change.as_ref()) {
        *target = change.into();
    }
}

fn required_assignment(patch: Option<&w::AssignmentPatch>) -> Option<ModelAssignment> {
    patch
        .and_then(|patch| patch.change.as_ref())
        .and_then(Into::into)
}

pub(crate) fn update(
    world: &mut World,
    request: w::UpdateCompanionRequest,
) -> Result<w::Companion> {
    let store = world.resource::<Store>().clone();
    let mut companion = store
        .companion_repo()
        .companion(&request.id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    if let Some(name) = request.name {
        companion.name = name;
    }
    if let Some(mind) = request.mind {
        companion.mind = mind;
    }
    if let Some(body) = request.body.as_option() {
        companion.body = wire::document_from(body);
    }
    if let Some(voice) = request.voice.as_option() {
        companion.voice = wire::document_from(voice);
    }
    if let Some(persona) = request.persona {
        companion.persona = Some(persona);
    }
    if let Some(voice) = request.voice_identifier {
        companion.voice_identifier = Some(voice);
    }
    if let Some(assignment) = required_assignment(request.companion_assignment.as_option()) {
        companion.assignments.companion = assignment;
    }
    if let Some(assignment) = required_assignment(request.task_assignment.as_option()) {
        companion.assignments.task = assignment;
    }
    set_assignment(
        &mut companion.assignments.perception,
        request.perception_assignment.as_option(),
    );
    set_assignment(
        &mut companion.assignments.compaction,
        request.compaction_assignment.as_option(),
    );
    set_assignment(
        &mut companion.assignments.consolidation,
        request.consolidation_assignment.as_option(),
    );
    set_assignment(
        &mut companion.assignments.decision,
        request.decision_assignment.as_option(),
    );
    set_assignment(
        &mut companion.assignments.embedding,
        request.embedding_assignment.as_option(),
    );
    companion.revision += 1;
    companion.updated_at = now();
    companion.validate()?;
    store
        .companion_repo()
        .update_companion(&companion, request.revision)?;
    agent::sync_companion(world, &companion.id)?;
    {
        let notifier = world.resource_mut::<Notifier>();
        notify(&notifier, &companion);
    }
    Ok(wire::companion(&companion))
}

pub(crate) fn delete(world: &mut World, id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let previous_client = store.client_repo().target(id, None)?;
    for session in store.work_repo().work_sessions(id)? {
        super::work::close(world, &session.id)?;
    }
    if !store.companion_repo().delete_companion(id)? {
        return Err(HostError::NotFound(("companion not found").into()));
    }
    if let Some(entity) = agent::agent_of(world, id) {
        world.trigger(Retire { entity });
    }
    if let Some(client) = previous_client {
        super::voice::on_client_moved(world.get_resource::<super::voice::Voice>(), id, &client);
    }
    world
        .resource_mut::<super::resources::Perception>()
        .events
        .remove(id);
    super::eyes::clear(&mut world.resource_mut::<super::eyes::Eyes>(), id);
    use super::client::dynamic::{PublicationChange, sync_tools};
    ene_domain::warn_if_err(sync_tools(world, PublicationChange::Refresh), "tool resync");
    world.resource::<Notifier>().push(
        Some(id),
        None,
        w::notification::Event::CompanionRemoved(Box::new(w::Id {
            id: id.to_owned(),
            ..Default::default()
        })),
    );
    Ok(())
}

pub(crate) fn notify(notifier: &Notifier, companion: &Companion) {
    let view = wire::companion(companion);
    notifier.push(
        Some(&companion.id),
        None,
        w::notification::Event::Companion(Box::new(view)),
    );
}

pub(crate) fn mind_view(store: &Store, id: &str) -> Result<w::MindView> {
    let companion = store
        .companion_repo()
        .companion(id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    Ok(w::MindView {
        companion_id: id.to_owned(),
        profile: Some(wire::mind_profile(&companion)).into(),
        state: Some(wire::mind_state(&companion)).into(),
        ..Default::default()
    })
}

pub(crate) fn update_profile(
    store: &Store,
    notifier: &Notifier,
    request: &w::UpdateMindProfileRequest,
) -> Result<w::MindView> {
    store
        .companion_repo()
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    let profile = request
        .profile
        .as_option()
        .map_or_default(wire::profile_from);
    store.companion_repo().update_mind_profile(
        &request.companion_id,
        request.expected_revision,
        &profile,
    )?;
    let view = mind_view(store, &request.companion_id)?;
    notify_mind(notifier, &view);
    Ok(view)
}

pub(crate) fn control_memory(
    store: &Store,
    notifier: &Notifier,
    request: &w::MemoryControlRequest,
) -> Result<w::MemoryControlResult> {
    let action = match request.action.as_known() {
        Some(w::MemoryAction::Forget) => super::db::memories::MemoryActionKind::Forget,
        Some(w::MemoryAction::Remember) => super::db::memories::MemoryActionKind::Remember,
        Some(w::MemoryAction::Share) => super::db::memories::MemoryActionKind::Share,
        Some(w::MemoryAction::Unshare) => super::db::memories::MemoryActionKind::Unshare,
        Some(w::MemoryAction::Unspecified) | None => {
            return Err(HostError::Invalid(("a memory action is required").into()));
        }
    };
    let memory = store
        .memory_repo()
        .memory(&request.memory_id)?
        .ok_or_else(|| HostError::NotFound(("memory not found").into()))?;
    if !memory.visible_to(&request.companion_id) {
        return Err(HostError::Permission(
            ("this memory is outside the companion's scope").into(),
        ));
    }
    let memory =
        store
            .memory_repo()
            .control_memory(&request.memory_id, action, &request.companion_id)?;
    notify_mind(notifier, &mind_view(store, &request.companion_id)?);
    Ok(w::MemoryControlResult {
        memory_id: memory.id,
        importance: u32::from(memory.importance),
        scope: memory.scope,
        revision: memory.memory_revision,
        ..Default::default()
    })
}

pub(crate) fn list_memories(
    store: &Store,
    request: &w::ListMemoriesRequest,
) -> Result<w::Memories> {
    store
        .companion_repo()
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    let limit = request.limit.unwrap_or(50).clamp(1, 200) as usize;
    let cursor = request
        .cursor
        .as_deref()
        .and_then(|cursor| cursor.parse::<i64>().ok());
    let (items, next) = store
        .memory_repo()
        .list_memories(&request.companion_id, cursor, limit)?;
    Ok(w::Memories {
        items: items.iter().map(wire::memory).collect(),
        next_cursor: next.map(|next| next.to_string()),
        ..Default::default()
    })
}

fn notify_mind(notifier: &Notifier, view: &w::MindView) {
    let companion = &view.companion_id;
    let profile = view
        .profile
        .as_option()
        .map_or_default(|profile| profile.revision);
    let state = view
        .state
        .as_option()
        .map_or_default(|state| state.revision);
    notifier.push(
        Some(companion),
        None,
        w::notification::Event::MindUpdated(Box::new(w::MindUpdated {
            companion_id: companion.to_owned(),
            profile_revision: profile,
            state_revision: state,
            updated_at: now(),
            ..Default::default()
        })),
    );
}

pub(crate) mod templates;

pub(crate) use templates::{
    export_template, import_template, list_templates, preview_system_prompt,
};

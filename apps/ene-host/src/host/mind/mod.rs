//! Companions, authored profiles and the current mind.
mod consolidate;
mod memory;
mod recall;
mod tools;

pub(crate) use self::{
    consolidate::{Batch, apply as apply_consolidation, start as start_consolidation},
    memory::control_memory,
    recall::inject_context,
    tools::register_tools,
};

use super::{
    agent::AgentKey,
    chat,
    companion::{Companion, Profile},
    db::{Result, Store, new_id, next_revision, now, validate_id},
    error::HostError,
    notify::Notifier,
    resources::ConsolidationQueue,
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;

pub(crate) const MAX_COMPANIONS: usize = 16;
const MAX_CONTENT: usize = 4000;
const MAX_TEXT: usize = 1000;
const MAX_ITEMS: usize = 16;

pub(crate) fn companions(store: &Store) -> Result<Vec<w::Companion>> {
    Ok(store.companions()?.iter().map(wire::companion).collect())
}

pub(crate) fn companion(store: &Store, id: &str) -> Result<Companion> {
    validate_id(id)?;
    store
        .companion(id)?
        .ok_or_else(|| HostError::not_found("companion not found"))
}

pub(crate) fn create_companion(
    world: &mut World,
    request: w::CreateCompanionRequest,
) -> Result<w::Companion> {
    if request.name.trim().is_empty() || request.name.chars().count() > 200 {
        return Err(HostError::invalid("invalid companion name"));
    }
    if request.mind.chars().count() > 16_000 {
        return Err(HostError::invalid("companion mind is too long"));
    }
    let store = world.resource::<Store>().clone();
    if store.companions()?.len() >= MAX_COMPANIONS {
        return Err(HostError::exhausted("too many companions"));
    }
    let mut record = Companion::seed(new_id(), now());
    record.name = request.name;
    record.mind = request.mind;
    record.body = wire::parse(&request.body)?;
    record.voice = wire::parse(&request.voice)?;
    record.assignments = assignments_from(request.assignments.as_option());
    record.persona = request.persona.clone();
    record.voice_identifier = request.voice_identifier.clone();
    if let Some(profile) = request.profile.as_option() {
        record.profile = Profile {
            facets: profile.facets.clone(),
            examples: profile.examples.clone(),
            background: profile.background.clone(),
            expression: profile.expression.clone(),
        };
    }
    store.save_companion(&record)?;
    notify_companion(world, &record);
    Ok(wire::companion(&record))
}

pub(crate) fn update_companion(
    world: &mut World,
    request: w::UpdateCompanionRequest,
) -> Result<w::Companion> {
    let store = world.resource::<Store>().clone();
    let mut record = companion(&store, &request.id)?;
    if record.revision != request.revision {
        return Err(HostError::conflict("companion revision changed"));
    }
    let mut changed = false;
    if let Some(name) = request.name {
        if name.trim().is_empty() || name.chars().count() > 200 {
            return Err(HostError::invalid("invalid companion name"));
        }
        changed |= record.name != name;
        record.name = name;
    }
    if let Some(mind) = request.mind {
        if mind.chars().count() > 16_000 {
            return Err(HostError::invalid("companion mind is too long"));
        }
        changed |= record.mind != mind;
        record.mind = mind;
    }
    if let Some(body) = request.body {
        let value = wire::parse(&body)?;
        changed |= record.body != value;
        record.body = value;
    }
    if let Some(voice) = request.voice {
        let value = wire::parse(&voice)?;
        changed |= record.voice != value;
        record.voice = value;
    }
    if let Some(patch) = request.companion_assignment.as_option() {
        let value = assignment_patch(patch)?;
        changed |= record.assignments.companion != value;
        record.assignments.companion = value;
    }
    if let Some(patch) = request.task_assignment.as_option() {
        let value = assignment_patch(patch)?;
        changed |= record.assignments.task != value;
        record.assignments.task = value;
    }
    if let Some(patch) = request.perception_assignment.as_option() {
        let value = assignment_patch(patch)?;
        changed |= record.assignments.perception != value;
        record.assignments.perception = value;
    }
    if let Some(patch) = request.compaction_assignment.as_option() {
        let value = assignment_patch(patch)?;
        changed |= record.assignments.compaction != value;
        record.assignments.compaction = value;
    }
    if let Some(patch) = request.consolidation_assignment.as_option() {
        let value = assignment_patch(patch)?;
        changed |= record.assignments.consolidation != value;
        record.assignments.consolidation = value;
    }
    if let Some(patch) = request.decision_assignment.as_option() {
        let value = assignment_patch(patch)?;
        changed |= record.assignments.decision != value;
        record.assignments.decision = value;
    }
    if let Some(patch) = request.embedding_assignment.as_option() {
        let value = assignment_patch(patch)?;
        changed |= record.assignments.embedding != value;
        record.assignments.embedding = value;
    }
    if let Some(persona) = request.persona {
        changed |= record.persona != Some(persona.clone());
        record.persona = Some(persona);
    }
    if let Some(voice) = request.voice_identifier {
        changed |= record.voice_identifier != Some(voice.clone());
        record.voice_identifier = Some(voice);
    }
    if changed {
        record.revision = next_revision(record.revision)?;
        record.updated_at = now();
        store.save_companion(&record)?;
        chat::refresh_prompt(world, &record.id);
        notify_companion(world, &record);
    }
    Ok(wire::companion(&record))
}

fn assignments_from(value: Option<&w::ModelAssignments>) -> super::companion::Assignments {
    let Some(value) = value else {
        return Default::default();
    };
    super::companion::Assignments {
        companion: value.companion.as_option().map(|assignment| {
            super::companion::ModelAssignment {
                connection: assignment.connection_id.clone(),
                model: assignment.model_id.clone(),
            }
        }),
        task: value.task.as_option().map(assignment),
        perception: value.perception.as_option().map(assignment),
        compaction: value.compaction.as_option().map(assignment),
        consolidation: value.consolidation.as_option().map(assignment),
        decision: value.decision.as_option().map(assignment),
        embedding: value.embedding.as_option().map(assignment),
    }
}

fn assignment_patch(
    patch: &w::AssignmentPatch,
) -> Result<Option<super::companion::ModelAssignment>> {
    match patch.change.as_ref() {
        Some(w::assignment_patch::Change::Set(assignment)) => {
            Ok(Some(super::companion::ModelAssignment {
                connection: assignment.connection_id.clone(),
                model: assignment.model_id.clone(),
            }))
        }
        Some(w::assignment_patch::Change::Clear(_)) => Ok(None),
        None => Err(HostError::invalid("empty assignment patch")),
    }
}

pub(crate) fn mind_view(store: &Store, id: &str) -> Result<w::MindView> {
    let record = companion(store, id)?;
    let cursor = store.consolidation(id)?.unwrap_or_default();
    Ok(w::MindView {
        companion_id: id.into(),
        profile: wire::mind_profile(&record).into(),
        state: wire::mind_state(&record).into(),
        index: Some(w::MindIndexStatus {
            pending_fts_count: 0,
            pending_vector_count: 0,
            embedding_model: String::new(),
            model_calls: 0,
            total_tokens: 0,
            ..Default::default()
        })
        .into(),
        consolidation: Some(w::MindConsolidationStatus {
            last_processed_turn_id: cursor.last_sequence.to_string(),
            jobs_completed: cursor.jobs,
            model_calls: cursor.model_calls,
            input_chars: cursor.input_chars,
            last_error: cursor.reason,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    })
}

pub(crate) fn update_mind_profile(
    world: &mut World,
    request: w::UpdateMindProfileRequest,
) -> Result<w::MindView> {
    let store = world.resource::<Store>().clone();
    let mut record = companion(&store, &request.companion_id)?;
    if record.profile_revision != request.expected_revision {
        return Err(HostError::conflict("profile revision changed"));
    }
    let profile = request
        .profile
        .as_option()
        .ok_or_else(|| HostError::invalid("profile is required"))?;
    let sections = [
        &profile.facets,
        &profile.examples,
        &profile.background,
        &profile.expression,
    ];
    if sections.iter().any(|list| list.len() > MAX_ITEMS) {
        return Err(HostError::invalid("too many profile items"));
    }
    if sections
        .into_iter()
        .flatten()
        .any(|item| item.chars().count() > 2000)
    {
        return Err(HostError::invalid("profile item is too long"));
    }
    record.profile = Profile {
        facets: profile.facets.clone(),
        examples: profile.examples.clone(),
        background: profile.background.clone(),
        expression: profile.expression.clone(),
    };
    record.profile_revision = next_revision(record.profile_revision)?;
    record.updated_at = now();
    store.save_companion(&record)?;
    chat::refresh_prompt(world, &record.id);
    let view = mind_view(&store, &record.id)?;
    notify_mind(world, &record.id, &view);
    Ok(view)
}

pub(crate) fn notify_companion(world: &mut World, companion: &Companion) {
    let view = wire::companion(companion);
    world.resource_mut::<Notifier>().push(
        Some(companion.id.clone()),
        None,
        w::notification::Event::Companion(Box::new(view)),
    );
}

pub(crate) fn notify_mind(world: &mut World, companion: &str, view: &w::MindView) {
    world.resource_mut::<Notifier>().push(
        Some(companion.into()),
        None,
        w::notification::Event::MindUpdated(Box::new(w::MindUpdated {
            companion_id: companion.into(),
            profile_revision: view
                .profile
                .as_option()
                .map_or(0, |profile| profile.revision),
            state_revision: view.state.as_option().map_or(0, |state| state.revision),
            updated_at: view
                .state
                .as_option()
                .map_or_else(String::new, |state| state.updated_at.clone()),
            ..Default::default()
        })),
    );
}

pub(crate) fn restore(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let mut queue = world.resource_mut::<ConsolidationQueue>();
    for companion in store.companions()? {
        queue.0.insert(companion.id);
    }
    Ok(())
}

fn assignment(value: &w::ModelAssignment) -> super::companion::ModelAssignment {
    super::companion::ModelAssignment {
        connection: value.connection_id.clone(),
        model: value.model_id.clone(),
    }
}

pub(crate) fn delete_companion(world: &mut World, id: &str) -> Result<()> {
    let agent = chat::find_agent(world, &AgentKey::parent(id));
    let store = world.resource::<Store>().clone();
    store
        .companion(id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    store.remove_companion(id)?;
    if let Some(agent) = agent {
        world.trigger(rig_ecs::session_log::Retire { entity: agent });
    }
    Ok(())
}

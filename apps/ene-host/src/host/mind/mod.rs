//! The companion: the author's profile, the mind it grew and its model
//! assignments. Query functions live in `db::companions`.
pub(crate) mod memory;

use super::error::{HostError, Result};
use super::json::JsonDoc;
use serde::{Deserialize, Serialize};

/// The seven model roles from the contract. `companion` and `task` are
/// required to run anything; the rest fall back as docs/design/work.md says.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Assignments {
    pub(crate) companion: Assignment,
    pub(crate) task: Assignment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) perception: Option<Assignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) compaction: Option<Assignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) consolidation: Option<Assignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) decision: Option<Assignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) embedding: Option<Assignment>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Assignment {
    pub(crate) connection_id: String,
    pub(crate) model_id: String,
}

impl Assignment {
    pub(crate) fn is_set(&self) -> bool {
        !self.connection_id.is_empty() && !self.model_id.is_empty()
    }
}

/// The profile material the author edits apart from the core.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Profile {
    #[serde(default)]
    pub(crate) facets: Vec<String>,
    #[serde(default)]
    pub(crate) examples: Vec<String>,
    #[serde(default)]
    pub(crate) background: Vec<String>,
    #[serde(default)]
    pub(crate) expression: Vec<String>,
}

/// The mind grown through interaction. Each part is a schema'd document so a
/// character keeps its own vocabulary and scale.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct State {
    pub(crate) emotion: JsonDoc,
    pub(crate) relationships: JsonDoc,
    pub(crate) tendencies: JsonDoc,
    pub(crate) concerns: JsonDoc,
}

impl Default for State {
    fn default() -> Self {
        Self {
            emotion: JsonDoc::new("ene.emotion.v1", serde_json::json!({})),
            relationships: JsonDoc::new("ene.relationships.v1", serde_json::json!([])),
            tendencies: JsonDoc::new("ene.tendencies.v1", serde_json::json!([])),
            concerns: JsonDoc::new("ene.concerns.v1", serde_json::json!([])),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Companion {
    pub(crate) id: String,
    pub(crate) name: String,
    /// The immutable core of the persona (`MindProfile.core`).
    pub(crate) mind: String,
    pub(crate) profile: Profile,
    pub(crate) profile_revision: u64,
    pub(crate) state: State,
    pub(crate) state_revision: u64,
    pub(crate) body: JsonDoc,
    pub(crate) voice: JsonDoc,
    pub(crate) assignments: Assignments,
    pub(crate) persona: Option<String>,
    pub(crate) voice_identifier: Option<String>,
    /// Optimistic concurrency for edits of the author's settings.
    pub(crate) revision: u64,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

impl Companion {
    pub(crate) fn validate(&self) -> Result<()> {
        super::db::validate_id(&self.id)?;
        if self.name.trim().is_empty() || self.name.chars().count() > 128 {
            return Err(HostError::invalid(
                "companion name must be 1-128 characters",
            ));
        }
        if self.mind.chars().count() > 8_000 {
            return Err(HostError::invalid("companion mind is too long"));
        }
        if self
            .persona
            .as_ref()
            .is_some_and(|text| text.chars().count() > 8_000)
        {
            return Err(HostError::invalid("companion persona is too long"));
        }
        if self
            .voice_identifier
            .as_ref()
            .is_some_and(|text| text.chars().count() > 128)
        {
            return Err(HostError::invalid("voice identifier is too long"));
        }
        self.body.validate()?;
        self.voice.validate()?;
        for text in [
            &self.profile.facets,
            &self.profile.examples,
            &self.profile.background,
            &self.profile.expression,
        ]
        .into_iter()
        .flatten()
        {
            if text.chars().count() > 4_000 {
                return Err(HostError::invalid("profile entry is too long"));
            }
        }
        Ok(())
    }

    /// The model assignment for one role, with the documented fallbacks.
    pub(crate) fn model(&self, role: Role) -> Option<&Assignment> {
        let assignments = &self.assignments;
        let chosen = match role {
            Role::Companion => Some(&assignments.companion),
            Role::Task => Some(&assignments.task),
            Role::Perception => assignments
                .perception
                .as_ref()
                .or(Some(&assignments.companion)),
            Role::Compaction => assignments
                .compaction
                .as_ref()
                .or(Some(&assignments.companion)),
            Role::Consolidation => assignments
                .consolidation
                .as_ref()
                .or(Some(&assignments.companion)),
            Role::Decision => assignments.decision.as_ref().or(Some(&assignments.task)),
            Role::Embedding => assignments.embedding.as_ref(),
        }?;
        chosen.is_set().then_some(chosen)
    }
}

// Model roles are addressed by name from the contract, so not every variant
// is constructed yet; `Companion::model` is what selects one.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Companion,
    Task,
    Perception,
    Compaction,
    Consolidation,
    Decision,
    Embedding,
}

// === Host operations ======================================================

pub(crate) mod recall;
pub(crate) mod tools;

use super::{
    agent,
    db::{Store, now},
    notify::Notifier,
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;

/// Registers the parent's memory tools and the dynamic-context systems.
pub(crate) struct MindPlugin;

impl Plugin for MindPlugin {
    fn build(&self, app: &mut App) {
        tools::register(app);
        app.add_plugins(recall::RecallPlugin);
    }
}

pub(crate) fn list(store: &Store) -> Result<Vec<w::Companion>> {
    Ok(store.companions()?.iter().map(wire::companion).collect())
}

pub(crate) fn get(store: &Store, id: &str) -> Result<w::Companion> {
    store
        .companion(id)?
        .map(|companion| wire::companion(&companion))
        .ok_or_else(|| HostError::not_found("companion not found"))
}

fn document_field(field: Option<&w::JsonDocument>) -> JsonDoc {
    field
        .map(wire::document_from)
        .unwrap_or_else(JsonDoc::empty)
}

pub(crate) fn create(
    world: &mut World,
    request: w::CreateCompanionRequest,
) -> Result<w::Companion> {
    let store = world.resource::<Store>().clone();
    if store.companions()?.len() >= 16 {
        return Err(HostError::exhausted("too many companions"));
    }
    let profile = request
        .profile
        .as_option()
        .map(wire::profile_from)
        .unwrap_or_default();
    let assignments = request
        .assignments
        .as_option()
        .map(wire::assignments_from)
        .unwrap_or_default();
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
    store.insert_companion(&companion)?;
    agent::spawn_companion(world, &companion);
    agent::sync_companion(world, &companion.id)?;
    notify(world, &companion);
    Ok(wire::companion(&companion))
}

fn set_assignment(target: &mut Option<Assignment>, patch: Option<&w::AssignmentPatch>) {
    match patch.and_then(|patch| patch.change.as_ref()) {
        Some(w::assignment_patch::Change::Set(set)) => {
            *target = Some(Assignment {
                connection_id: set.connection_id.clone(),
                model_id: set.model_id.clone(),
            });
        }
        Some(w::assignment_patch::Change::Clear(_)) => *target = None,
        None => {}
    }
}

fn required_assignment(patch: Option<&w::AssignmentPatch>) -> Option<Assignment> {
    match patch.and_then(|patch| patch.change.as_ref()) {
        Some(w::assignment_patch::Change::Set(set)) => Some(Assignment {
            connection_id: set.connection_id.clone(),
            model_id: set.model_id.clone(),
        }),
        _ => None,
    }
}

pub(crate) fn update(
    world: &mut World,
    request: w::UpdateCompanionRequest,
) -> Result<w::Companion> {
    let store = world.resource::<Store>().clone();
    let mut companion = store
        .companion(&request.id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
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
    store.update_companion(&companion, request.revision)?;
    agent::sync_companion(world, &companion.id)?;
    notify(world, &companion);
    Ok(wire::companion(&companion))
}

pub(crate) fn delete(world: &mut World, id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    if !store.delete_companion(id)? {
        return Err(HostError::not_found("companion not found"));
    }
    if let Some(entity) = agent::agent_of(world, id) {
        world.trigger(Retire { entity });
    }
    Ok(())
}

fn notify(world: &mut World, companion: &Companion) {
    let view = wire::companion(companion);
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.push(
        Some(&companion.id),
        None,
        w::notification::Event::Companion(Box::new(view)),
    );
}

pub(crate) fn mind_view(store: &Store, id: &str) -> Result<w::MindView> {
    let companion = store
        .companion(id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let cursor = store.consolidation_cursor(id)?;
    Ok(w::MindView {
        companion_id: id.to_owned(),
        profile: Some(wire::mind_profile(&companion)).into(),
        state: Some(wire::mind_state(&companion)).into(),
        index: None.into(),
        consolidation: cursor
            .map(|cursor| w::MindConsolidationStatus {
                last_processed_turn_id: cursor,
                ..Default::default()
            })
            .into(),
        ..Default::default()
    })
}

pub(crate) fn update_profile(
    world: &mut World,
    request: w::UpdateMindProfileRequest,
) -> Result<w::MindView> {
    let store = world.resource::<Store>().clone();
    store
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let profile = request
        .profile
        .as_option()
        .map(wire::profile_from)
        .unwrap_or_default();
    store.update_mind_profile(&request.companion_id, request.expected_revision, &profile)?;
    notify_mind(world, &request.companion_id);
    mind_view(&store, &request.companion_id)
}

pub(crate) fn control_memory(
    world: &mut World,
    request: w::MemoryControlRequest,
) -> Result<w::MemoryControlResult> {
    let store = world.resource::<Store>().clone();
    let action = match request.action.to_i32() {
        1 => super::db::memories::MemoryActionKind::Forget,
        2 => super::db::memories::MemoryActionKind::Remember,
        3 => super::db::memories::MemoryActionKind::Share,
        4 => super::db::memories::MemoryActionKind::Unshare,
        _ => return Err(HostError::invalid("a memory action is required")),
    };
    let memory = store
        .memory(&request.memory_id)?
        .ok_or_else(|| HostError::not_found("memory not found"))?;
    if memory.owner != request.companion_id {
        return Err(HostError::permission(
            "only the owner may change a memory's importance or audience",
        ));
    }
    let memory = store.control_memory(&request.memory_id, action, None)?;
    notify_mind(world, &request.companion_id);
    Ok(w::MemoryControlResult {
        memory_id: memory.id,
        importance: u32::from(memory.importance),
        audiences: memory.audiences,
        revision: memory.memory_revision,
        ..Default::default()
    })
}

pub(crate) fn list_memories(
    world: &mut World,
    request: w::ListMemoriesRequest,
) -> Result<w::Memories> {
    let store = world.resource::<Store>().clone();
    store
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let limit = request.limit.unwrap_or(50).clamp(1, 200) as usize;
    let cursor = request
        .cursor
        .as_deref()
        .and_then(|cursor| cursor.parse::<i64>().ok());
    let (items, next) = store.list_memories(&request.companion_id, cursor, limit)?;
    let _ = world;
    Ok(w::Memories {
        items: items.iter().map(wire::memory).collect(),
        next_cursor: next.map(|next| next.to_string()),
        ..Default::default()
    })
}

fn notify_mind(world: &mut World, companion: &str) {
    let store = world.resource::<Store>().clone();
    let Ok(view) = mind_view(&store, companion) else {
        return;
    };
    let profile = view
        .profile
        .as_option()
        .map(|profile| profile.revision)
        .unwrap_or_default();
    let state = view
        .state
        .as_option()
        .map(|state| state.revision)
        .unwrap_or_default();
    let mut notifier = world.resource_mut::<Notifier>();
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

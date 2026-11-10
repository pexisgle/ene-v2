//! Companions, author-defined MindProfile and evolving MindState.
mod consolidate;
mod memory;
mod summary;
mod tools;

pub(crate) use self::{
    consolidate::{Batch, Consolidation, apply_consolidation, restore, start_consolidation},
    memory::{FORGOTTEN, MemoryRecord, NORMAL, PRIVATE, SHARED, control_memory, inject_recall},
    summary::{
        Job as SummaryJob, Summary, apply as apply_summary, inject as inject_summary,
        start as start_summary,
    },
    tools::register_tools,
};

use super::{
    Host, chat,
    db::{
        Database, Result, aborted, document, invalid, new_id, next_revision, not_found, now,
        validate_id,
    },
};
use bevy_ecs::prelude::*;
use connectrpc::ConnectError;
use ene_rpc as w;
use rig_core::conversation::{Author, RequestId};
use rig_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The number of companions a host serves, advertised and enforced.
pub(crate) const MAX_COMPANIONS: usize = 16;
pub(super) const MAX_CONTENT: usize = 4000;
pub(super) const MAX_TEXT: usize = 1000;
pub(super) const MAX_ITEMS: usize = 16;

/// How a memory came to the companion: the only values the host stores.
pub(crate) const ACQUISITIONS: &[&str] = &[
    "direct_experience",
    "user_statement",
    "report",
    "shared",
    "inference",
];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProfileData {
    #[serde(default)]
    pub(crate) facets: Vec<String>,
    #[serde(default)]
    pub(crate) examples: Vec<String>,
    #[serde(default)]
    pub(crate) background: Vec<String>,
    #[serde(default)]
    pub(crate) expression: Vec<String>,
}

impl ProfileData {
    pub(crate) fn sections(&self) -> [(&'static str, &[String]); 4] {
        [
            ("facets", &self.facets),
            ("examples", &self.examples),
            ("background", &self.background),
            ("expression", &self.expression),
        ]
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StateData {
    #[serde(default)]
    pub(crate) emotion: Value,
    #[serde(default)]
    pub(crate) relationships: Vec<Value>,
    #[serde(default)]
    pub(crate) tendencies: Vec<String>,
    #[serde(default)]
    pub(crate) concerns: Vec<String>,
}

impl StateData {
    pub(crate) fn is_empty(&self) -> bool {
        self.emotion.is_null()
            && self.relationships.is_empty()
            && self.tendencies.is_empty()
            && self.concerns.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CompanionRecord {
    pub(crate) companion: w::Companion,
    pub(crate) profile: ProfileData,
    pub(crate) state: StateData,
    pub(crate) profile_revision: u64,
    pub(crate) state_revision: u64,
    pub(crate) memory_revision: u64,
}

impl CompanionRecord {
    pub(crate) fn seed(id: String, created_at: String) -> Self {
        Self {
            companion: w::Companion {
                id,
                name: "ene".into(),
                mind: "日本語で、簡潔で親しみやすく応答する。".into(),
                body: document(json!({})).into(),
                voice: document(json!({})).into(),
                assignments: w::ModelAssignments::default().into(),
                revision: 1,
                created_at: created_at.clone(),
                updated_at: created_at,
                active_client: None,
                activation_revision: 1,
                ..Default::default()
            },
            profile: ProfileData::default(),
            state: StateData::default(),
            profile_revision: 1,
            state_revision: 1,
            memory_revision: 1,
        }
    }

    pub(crate) fn mind(&self) -> w::MindProfile {
        w::MindProfile {
            core: self.companion.mind.clone(),
            revision: self.profile_revision,
            facets: self.profile.facets.clone(),
            examples: self.profile.examples.clone(),
            background: self.profile.background.clone(),
            expression: self.profile.expression.clone(),
            ..Default::default()
        }
    }

    pub(crate) fn mind_state(&self) -> w::MindState {
        w::MindState {
            revision: self.state_revision,
            emotion: document(self.state.emotion.clone()).into(),
            relationships: document(json!(self.state.relationships)).into(),
            tendencies: document(json!(self.state.tendencies)).into(),
            concerns: document(json!(self.state.concerns)).into(),
            updated_at: self.companion.updated_at.clone(),
            ..Default::default()
        }
    }
}

pub(crate) fn companions(database: &Database) -> Result<Vec<w::Companion>> {
    Ok(database
        .list::<CompanionRecord>("companion:")?
        .into_iter()
        .map(|(_, record)| record.companion)
        .collect())
}

pub(crate) fn companion(database: &Database, id: &str) -> Result<CompanionRecord> {
    validate_id(id)?;
    database
        .get(&format!("companion:{id}"))?
        .ok_or_else(|| not_found("companion not found"))
}

pub(crate) fn create_companion(
    world: &mut World,
    request: w::CreateCompanionRequest,
) -> Result<w::Companion> {
    if request.name.trim().is_empty() || request.name.chars().count() > 200 {
        return Err(invalid("invalid companion name"));
    }
    if request.mind.chars().count() > 16_000 {
        return Err(invalid("companion mind is too long"));
    }
    if companions(&world.resource::<Host>().db)?.len() >= MAX_COMPANIONS {
        return Err(ConnectError::resource_exhausted("too many companions"));
    }
    let mut record = CompanionRecord::seed(new_id(), now());
    record.companion.name = request.name;
    record.companion.mind = request.mind;
    record.companion.body = request.body;
    record.companion.voice = request.voice;
    record.companion.assignments = request.assignments;
    if let Some(profile) = request.profile.into_option() {
        record.profile = ProfileData {
            facets: profile.facets,
            examples: profile.examples,
            background: profile.background,
            expression: profile.expression,
        };
    }
    world
        .resource::<Host>()
        .db
        .put(&format!("companion:{}", record.companion.id), &record)?;
    notify_companion(world, &record.companion);
    Ok(record.companion)
}

pub(crate) fn update_companion(
    world: &mut World,
    request: w::UpdateCompanionRequest,
) -> Result<w::Companion> {
    let db = &world.resource::<Host>().db;
    let mut record = companion(db, &request.id)?;
    if record.companion.revision != request.revision {
        return Err(aborted("companion revision changed"));
    }
    if let Some(name) = request.name {
        if name.trim().is_empty() || name.chars().count() > 200 {
            return Err(invalid("invalid companion name"));
        }
        record.companion.name = name;
    }
    if let Some(mind) = request.mind {
        if mind.chars().count() > 16_000 {
            return Err(invalid("companion mind is too long"));
        }
        record.companion.mind = mind;
    }
    if let Some(body) = request.body.into_option() {
        record.companion.body = document(patch_doc(body)?).into();
    }
    if let Some(voice) = request.voice.into_option() {
        record.companion.voice = document(patch_doc(voice)?).into();
    }
    if let Some(patch) = request.companion_assignment.into_option() {
        apply_assignment(&mut record, true, patch)?;
    }
    if let Some(patch) = request.task_assignment.into_option() {
        apply_assignment(&mut record, false, patch)?;
    }
    record.companion.revision = next_revision(record.companion.revision)?;
    record.companion.updated_at = now();
    db.put(&format!("companion:{}", record.companion.id), &record)?;
    chat::refresh_prompt(world, &record.companion.id);
    notify_companion(world, &record.companion);
    Ok(record.companion)
}

fn patch_doc(patch: w::JsonDocumentPatch) -> Result<Value> {
    match patch.change {
        Some(w::json_document_patch::Change::Set(doc)) => {
            serde_json::from_str(&doc.json).map_err(|_| invalid("invalid document"))
        }
        Some(w::json_document_patch::Change::Clear(_)) => Ok(json!({})),
        None => Err(invalid("empty document patch")),
    }
}

fn apply_assignment(
    record: &mut CompanionRecord,
    companion: bool,
    patch: w::AssignmentPatch,
) -> Result<()> {
    let mut assignments = record
        .companion
        .assignments
        .as_option()
        .cloned()
        .unwrap_or_default();
    let value = match patch.change {
        Some(w::assignment_patch::Change::Set(assignment)) => (*assignment).into(),
        Some(w::assignment_patch::Change::Clear(_)) => Default::default(),
        None => return Err(invalid("empty assignment patch")),
    };
    if companion {
        assignments.companion = value;
    } else {
        assignments.task = value;
    }
    record.companion.assignments = assignments.into();
    Ok(())
}

pub(crate) fn mind_view(database: &Database, id: &str) -> Result<w::MindView> {
    let record = companion(database, id)?;
    let cursor = database
        .get::<Consolidation>(&format!("memory-cursor:{id}"))?
        .unwrap_or_default();
    let memories = database.count_memories(id)?;
    Ok(w::MindView {
        companion_id: id.into(),
        profile: record.mind().into(),
        state: record.mind_state().into(),
        memory_revision: record.memory_revision,
        index: w::MemoryStatus {
            state: "ready".into(),
            reason: None,
            memories,
            semantic: false,
            updated_at: cursor.updated_at.clone(),
            ..Default::default()
        }
        .into(),
        consolidation: w::ConsolidationStatus {
            last_sequence: cursor.last_sequence,
            last_offset: cursor.last_offset,
            state: cursor.state,
            reason: cursor.reason,
            jobs: cursor.jobs,
            model_calls: cursor.model_calls,
            input_chars: cursor.input_chars,
            skipped: cursor.skipped,
            updated_at: cursor.updated_at,
            ..Default::default()
        }
        .into(),
        ..Default::default()
    })
}

pub(crate) fn update_mind_profile(
    world: &mut World,
    request: w::UpdateMindProfileRequest,
) -> Result<w::MindView> {
    let view = {
        let db = &world.resource::<Host>().db;
        let mut record = companion(db, &request.companion_id)?;
        if record.profile_revision != request.expected_revision {
            return Err(aborted("profile revision changed"));
        }
        let sections = [
            &request.facets,
            &request.examples,
            &request.background,
            &request.expression,
        ];
        if sections.iter().any(|list| list.len() > MAX_ITEMS) {
            return Err(invalid("too many profile items"));
        }
        if sections
            .into_iter()
            .flatten()
            .any(|item| item.chars().count() > 2000)
        {
            return Err(invalid("profile item is too long"));
        }
        record.profile = ProfileData {
            facets: request.facets,
            examples: request.examples,
            background: request.background,
            expression: request.expression,
        };
        record.profile_revision = next_revision(record.profile_revision)?;
        record.companion.updated_at = now();
        db.put(&format!("companion:{}", record.companion.id), &record)?;
        mind_view(db, &record.companion.id)?
    };
    chat::refresh_prompt(world, &view.companion_id);
    notify_mind(world, &view.companion_id, view.clone());
    Ok(view)
}

pub(crate) fn deliver_state(world: &mut World, agent: Entity, companion_id: &str) -> Result<()> {
    let record = companion(&world.resource::<Host>().db, companion_id)?;
    let request_id = format!("mind:{}", record.state_revision);
    if chat::has_request(world, agent, &request_id) {
        return Ok(());
    }
    // Even an empty mind is stated, so the model knows the revision it must
    // name when it updates it.
    let text = if record.state.is_empty() {
        format!(
            "現在の心（revision {}）: （まだ何もありません）",
            record.state_revision
        )
    } else {
        let state_json = serde_json::to_string(&record.state).unwrap_or_default();
        format!(
            "現在の心（revision {}）:\n{state_json}",
            record.state_revision
        )
    };
    let author = Author {
        request: Some(RequestId(request_id)),
        ..Author::plugin("mind")
    };
    world.trigger(Deliver::new(agent, text, DeliveryMode::Inform).with_author(author));
    Ok(())
}

fn notify_companion(world: &mut World, companion: &w::Companion) {
    world.resource_mut::<Host>().notify(
        Some(companion.id.clone()),
        None,
        w::notification::Event::CompanionUpdated(Box::new(w::CompanionUpdated {
            companion: companion.clone().into(),
            ..Default::default()
        })),
    );
}

pub(crate) fn notify_mind(world: &mut World, companion: &str, view: w::MindView) {
    world.resource_mut::<Host>().notify(
        Some(companion.into()),
        None,
        w::notification::Event::MindUpdated(Box::new(w::MindUpdated {
            mind: view.into(),
            ..Default::default()
        })),
    );
}

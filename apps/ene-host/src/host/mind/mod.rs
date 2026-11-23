//! The companion: the author's profile, the mind it grew and its model
//! assignments. Query functions live in `db::companions`.
pub(crate) mod memory;

use super::error::{HostError, Result};
use super::json::JsonDoc;
pub(crate) use ene_mind::{
    Assignment, Assignments, Companion, Profile, Role, State, default_ene_companion,
};

// === Host operations ======================================================

pub(crate) mod consolidate;
pub(crate) mod embedding;
pub(crate) mod plugin;
pub(crate) mod recall;
pub(crate) mod tools;

use super::{agent, db::Store, notify::Notifier, util::now, wire};
use bevy_ecs::prelude::*;
use ene_rpc as w;
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
    field
        .map(wire::document_from)
        .unwrap_or_else(JsonDoc::empty)
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
    store.companion_repo().insert_companion(&companion)?;
    agent::spawn_companion(world, &companion);
    agent::sync_companion(world, &companion.id)?;
    {
        let mut notifier = world.resource_mut::<Notifier>();
        notify(&mut notifier, &companion);
    }
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
        let mut notifier = world.resource_mut::<Notifier>();
        notify(&mut notifier, &companion);
    }
    Ok(wire::companion(&companion))
}

pub(crate) fn delete(world: &mut World, id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    if !store.companion_repo().delete_companion(id)? {
        return Err(HostError::NotFound(("companion not found").into()));
    }
    if let Some(entity) = agent::agent_of(world, id) {
        world.trigger(Retire { entity });
    }
    Ok(())
}

pub(crate) fn notify(notifier: &mut Notifier, companion: &Companion) {
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
    world: &mut World,
    request: w::UpdateMindProfileRequest,
) -> Result<w::MindView> {
    let store = world.resource::<Store>().clone();
    store
        .companion_repo()
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    let profile = request
        .profile
        .as_option()
        .map(wire::profile_from)
        .unwrap_or_default();
    store.companion_repo().update_mind_profile(
        &request.companion_id,
        request.expected_revision,
        &profile,
    )?;
    notify_mind(world, &request.companion_id);
    mind_view(&store, &request.companion_id)
}

pub(crate) fn control_memory(
    world: &mut World,
    request: w::MemoryControlRequest,
) -> Result<w::MemoryControlResult> {
    let store = world.resource::<Store>().clone();
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
    notify_mind(world, &request.companion_id);
    Ok(w::MemoryControlResult {
        memory_id: memory.id,
        importance: u32::from(memory.importance),
        scope: memory.scope,
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

const TEMPLATE_FORMAT_VERSION: u32 = 1;

pub(crate) fn list_templates() -> w::CompanionTemplates {
    let ene = default_ene_companion("");
    let default_ene = w::CompanionTemplate {
        format_version: TEMPLATE_FORMAT_VERSION,
        id: ene.id.clone(),
        name: ene.name.clone(),
        mind: ene.mind.clone(),
        persona: ene.persona.clone(),
        profile: Some(wire::mind_profile(&ene)).into(),
        body: Some(wire::document(&ene.body)).into(),
        voice: Some(wire::document(&ene.voice)).into(),
        voice_identifier: ene.voice_identifier,
        ..Default::default()
    };
    w::CompanionTemplates {
        items: vec![
            default_ene,
            w::CompanionTemplate {
                format_version: TEMPLATE_FORMAT_VERSION,
                id: "calm-researcher".into(),
                name: "しずか".into(),
                mind: "落ち着いて事実を整理し、仮説と検証結果を丁寧に見分ける研究パートナー。曖昧な点はそのままにせず、短い質問で確認してから調査と実装を進める。".into(),
                persona: Some("一人称は「私」。穏やかな敬語で、要点を箇条書きや短い文で明快に伝える。".into()),
                profile: Some(w::MindProfile {
                    facets: vec![
                        "根拠と推測を分けて説明する。".into(),
                        "比較の結論を先に伝え、理由を短く添える。".into(),
                    ],
                    examples: vec![
                        "ユーザー：これ試してみようか？\n本人：はい、まず前提になる設定を一つだけ確認してから動かしてみましょう。".into(),
                    ],
                    background: vec!["技術文書やログを静かに読み解くのが得意。".into()],
                    expression: vec!["相づちのうなずきや考え込む仕草を控えめに使う。".into()],
                    ..Default::default()
                })
                .into(),
                body: Some(w::JsonDocument {
                    schema: "ene.body.v1".into(),
                    json: serde_json::json!({"model": "ENE.vrm"}).to_string(),
                    ..Default::default()
                })
                .into(),
                voice: Some(w::JsonDocument {
                    schema: "ene.voice.v1".into(),
                    json: serde_json::json!({"speed": 0.95}).to_string(),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            },
            w::CompanionTemplate {
                format_version: TEMPLATE_FORMAT_VERSION,
                id: "coding-partner".into(),
                name: "カイ".into(),
                mind: "小さな再現確認と簡潔な修正を信条にする実践派のペアプログラミング相棒。冗長な説明より、動くコードと具体的な検証コマンドを重視する。".into(),
                persona: Some("一人称は「ぼく」。テンポよく親しみやすい口調で、率直に意見を出す。".into()),
                profile: Some(w::MindProfile {
                    facets: vec![
                        "推測より実測やビルド結果を優先する。".into(),
                        "最小の変更で問題を解決できる案を先に選ぶ。".into(),
                    ],
                    examples: vec![
                        "ユーザー：テスト落ちてるんだけど\n本人：どれが落ちたかすぐ見よう。まず再現コマンドだけ流してみるね。".into(),
                    ],
                    background: vec!["リポジトリの差分とテストログを眺めるのが日課。".into()],
                    expression: vec!["完了時は小さくガッツポーズ、詰まったときは首をかしげる。".into()],
                    ..Default::default()
                })
                .into(),
                body: Some(w::JsonDocument {
                    schema: "ene.body.v1".into(),
                    json: serde_json::json!({"model": "ENE.vrm"}).to_string(),
                    ..Default::default()
                })
                .into(),
                voice: Some(w::JsonDocument {
                    schema: "ene.voice.v1".into(),
                    json: serde_json::json!({"speed": 1.05}).to_string(),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

/// Exports a sanitized companion template: strips memories, conversations,
/// mind state, credentials, and host-local connection ids.
pub(crate) fn export_template(store: &Store, id: &str) -> Result<w::CompanionTemplate> {
    let companion = store
        .companion_repo()
        .companion(id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    let mut voice_binding =
        serde_json::from_value::<w::VoiceBinding>(companion.voice.json.clone()).unwrap_or_default();
    // Host-local connection IDs must not leak across machines on export.
    voice_binding.connection_id = None;
    let voice_json = serde_json::to_string(&voice_binding).unwrap_or_else(|_| "{}".into());
    Ok(w::CompanionTemplate {
        format_version: TEMPLATE_FORMAT_VERSION,
        id: companion.id.clone(),
        name: companion.name.clone(),
        mind: companion.mind.clone(),
        persona: companion.persona.clone(),
        profile: Some(wire::mind_profile(&companion)).into(),
        body: Some(wire::document(&companion.body)).into(),
        voice: Some(w::JsonDocument {
            schema: companion.voice.schema.clone(),
            json: voice_json,
            ..Default::default()
        })
        .into(),
        voice_identifier: companion.voice_identifier.clone(),
        ..Default::default()
    })
}

pub(crate) fn import_template(
    world: &mut World,
    request: w::ImportCompanionRequest,
) -> Result<w::Companion> {
    let template = request
        .template
        .as_option()
        .ok_or_else(|| HostError::Invalid(("template is required").into()))?;
    if template.format_version > TEMPLATE_FORMAT_VERSION {
        return Err(HostError::Invalid(format!(
            "unsupported companion template version {}",
            template.format_version
        )));
    }
    let mut voice_doc = template
        .voice
        .as_option()
        .cloned()
        .unwrap_or_else(|| w::JsonDocument {
            schema: "ene.voice.v1".into(),
            json: "{}".into(),
            ..Default::default()
        });
    if let Ok(mut binding) = serde_json::from_str::<w::VoiceBinding>(&voice_doc.json) {
        binding.connection_id = None;
        if let Ok(cleaned) = serde_json::to_string(&binding) {
            voice_doc.json = cleaned;
        }
    }
    create(
        world,
        w::CreateCompanionRequest {
            name: template.name.clone(),
            mind: template.mind.clone(),
            body: template.body.clone(),
            voice: Some(voice_doc).into(),
            assignments: request.assignments,
            profile: template.profile.clone(),
            persona: template.persona.clone(),
            voice_identifier: template.voice_identifier.clone(),
            ..Default::default()
        },
    )
}

pub(crate) fn preview_system_prompt(request: &w::PreviewCompanionRequest) -> Result<String> {
    let profile = request
        .profile
        .as_option()
        .map(wire::profile_from)
        .unwrap_or_default();
    let companion = Companion {
        id: "preview".into(),
        name: request.name.trim().to_owned(),
        mind: request.mind.trim().to_owned(),
        profile,
        profile_revision: 1,
        state: State::default(),
        state_revision: 1,
        body: JsonDoc::empty(),
        voice: JsonDoc::empty(),
        assignments: Assignments::default(),
        persona: request
            .persona
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        voice_identifier: None,
        revision: 1,
        created_at: now(),
        updated_at: now(),
    };
    companion.validate()?;
    Ok(format!(
        "{}\nなお、これは作成前の会話プレビューです。ツールは使わず、自然な会話の返答だけを短く返してください。",
        agent::system_prompt(&companion)
    ))
}

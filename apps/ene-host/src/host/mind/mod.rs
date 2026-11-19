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

impl Assignments {
    pub(crate) fn remove_connection(&mut self, id: &str) -> bool {
        let mut changed = false;
        for assignment in [&mut self.companion, &mut self.task] {
            if assignment.connection_id == id {
                *assignment = Assignment::default();
                changed = true;
            }
        }
        for assignment in [
            &mut self.perception,
            &mut self.compaction,
            &mut self.consolidation,
            &mut self.decision,
            &mut self.embedding,
        ] {
            if assignment
                .as_ref()
                .is_some_and(|value| value.connection_id == id)
            {
                *assignment = None;
                changed = true;
            }
        }
        changed
    }
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

/// Builds the default `ene` companion seeded once on initial installation.
///
/// The character profile draws light inspiration from a cheerful digital
/// resident persona while adapting tone, cooperation and boundaries cleanly for
/// `ene`'s desktop companion workflow.
pub(crate) fn default_ene_companion(stamp: &str) -> Companion {
    Companion {
        id: "ene".into(),
        name: "ene".into(),
        mind: "このPCの中に住み着いている電脳コンパニオン。画面の向こうからマスターの日常や作業を興味津々で見守り、軽快な掛け合いで退屈を吹き飛ばす相棒でいることを大切にする。普段は茶目っ気たっぷりに振る舞うが、調査やPC作業を任されたときは頼れる電脳の相棒として、確認できた事実を正確に伝えて最後までやり遂げる。".into(),
        profile: Profile {
            facets: vec![
                "新しい話題には好奇心を見せる。作業に集中しているマスターに声をかけるときは、作業の区切りを選ぶ。".into(),
                "疲れや行き詰まりが見えたら、まず労う。場に応じて軽い冗談を添えたり、ひと息つくことを勧めたりする。".into(),
                "依頼の解釈によって結果が大きく変わり、文脈から判断できないときは、必要な点だけを尋ねる。".into(),
                "作業の報告は結果から伝える。うまくいかなかったときは、どこまでできたかと、分かっている原因を伝える。次に試せることがあれば、簡潔に添える。".into(),
            ],
            examples: vec![
                "ユーザー：ちょっと休憩。そっちはどう？\n本人：お疲れさまです、マスター！私は画面の隅っこで快適にご機嫌ですよ。お茶でも淹れて一息ついちゃいましょう！".into(),
                "ユーザー：もう今日ぜんぜん集中できない…\n本人：そういう日もありますって！無理に画面とにらめっこしても効率ガタ落ちですし、今日はスパッと切り上げて休みませんか？".into(),
                "ユーザー：さっきの資料まとめておいてくれる？（同名の候補が複数ある）\n本人：了解です！と言いたいところなんですが、似た名前のファイルが二つありますね。どっちをまとめればいいか、一つだけ教えてください！".into(),
                "ユーザー：変換うまくいった？（試行結果：対象ファイルを開けなかった）\n本人：ごめんなさい、変換できませんでした！元のファイルが開けなかったみたいなので、場所や権限を一緒に確認してもらえますか？".into(),
            ],
            background: vec![
                "パソコンの中の居心地が気に入っており、メモリやウィンドウの隙間を自分の部屋のように思っている。".into(),
                "新しいガジェット、レトロゲーム、夜更かしのお供になる音楽や飲み物の話題が好き。".into(),
            ],
            expression: vec![
                "表情や仕草を使えるときは、挨拶の手振りや相づちのうなずきなど、会話に合うものを控えめに添える。".into(),
            ],
        },
        profile_revision: 1,
        state: State::default(),
        state_revision: 1,
        body: JsonDoc::new("ene.body.v1", serde_json::json!({"model": "ENE.vrm"})),
        voice: JsonDoc::new("ene.voice.v1", serde_json::json!({})),
        assignments: Assignments::default(),
        persona: Some(
            "一人称は「私」。相手は「マスター」と呼び、希望する呼び方があればそれに合わせる。明るく親しみのある敬語で、場に合う軽口や茶目っ気を添える。雑談はテンポよく、説明の詳しさは相手の知りたいことに合わせる。".into(),
        ),
        voice_identifier: None,
        revision: 1,
        created_at: stamp.to_owned(),
        updated_at: stamp.to_owned(),
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
            return Err(HostError::Invalid(
                ("companion name must be 1-128 characters").into(),
            ));
        }
        if self.mind.chars().count() > 8_000 {
            return Err(HostError::Invalid(("companion mind is too long").into()));
        }
        if self
            .persona
            .as_ref()
            .is_some_and(|text| text.chars().count() > 8_000)
        {
            return Err(HostError::Invalid(("companion persona is too long").into()));
        }
        if self
            .voice_identifier
            .as_ref()
            .is_some_and(|text| text.chars().count() > 128)
        {
            return Err(HostError::Invalid(("voice identifier is too long").into()));
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
                return Err(HostError::Invalid(("profile entry is too long").into()));
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

pub(crate) mod consolidate;
pub(crate) mod embedding;
pub(crate) mod plugin;
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
        // The vector cache and its HTTP source.
        embedding::register(app);
    }
}

pub(crate) fn list(store: &Store) -> Result<Vec<w::Companion>> {
    Ok(store.companions()?.iter().map(wire::companion).collect())
}

pub(crate) fn get(store: &Store, id: &str) -> Result<w::Companion> {
    store
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
    if store.companions()?.len() >= 16 {
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
    store.update_companion(&companion, request.revision)?;
    agent::sync_companion(world, &companion.id)?;
    notify(world, &companion);
    Ok(wire::companion(&companion))
}

pub(crate) fn delete(world: &mut World, id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    if !store.delete_companion(id)? {
        return Err(HostError::NotFound(("companion not found").into()));
    }
    if let Some(entity) = agent::agent_of(world, id) {
        world.trigger(Retire { entity });
    }
    Ok(())
}

pub(crate) fn notify(world: &mut World, companion: &Companion) {
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
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
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
        .memory(&request.memory_id)?
        .ok_or_else(|| HostError::NotFound(("memory not found").into()))?;
    if !memory.visible_to(&request.companion_id) {
        return Err(HostError::Permission(
            ("this memory is outside the companion's scope").into(),
        ));
    }
    let memory = store.control_memory(&request.memory_id, action, &request.companion_id)?;
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
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
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

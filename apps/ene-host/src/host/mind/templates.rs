//! Companion templates: the built-ins, and the export/import pair that carries
//! one companion between machines with host-local state stripped, plus the
//! pre-create conversation preview.
use super::{Companion, ModelAssignments, State, create, default_ene_companion};
use crate::host::{
    db::{Result, Store},
    error::HostError,
    wire,
};
use bevy_ecs::prelude::*;
use ene_mind::JsonDoc;
use ene_rpc as w;
use ene_store::util::now;
use serde::Deserialize as _;

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
        ene_domain::VoiceBinding::deserialize(&companion.voice.json).unwrap_or_default();
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
    if let Ok(mut binding) = serde_json::from_str::<ene_domain::VoiceBinding>(&voice_doc.json) {
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
        .map_or_default(wire::profile_from);
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
        assignments: ModelAssignments::default(),
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
        ene_mind::prompt::system_prompt(&companion)
    ))
}

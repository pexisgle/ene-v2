//! Companion profile, template, preview, model role, and mind operations on [`Hub`].

use super::{Hub, ModelRoleSlot, NewCompanionSpec, companion_update, decode, workflows};
use crate::{state::NoticeKind, update::Update};
use buffa::view::MessageView;
use ene_rpc as w;
use gpui::Context;

/// Profile update payload passed to [`Hub::update_companion_profile`].
pub(crate) struct CompanionProfilePatch {
    pub(crate) companion_id: String,
    pub(crate) name: String,
    pub(crate) persona: String,
    pub(crate) mind: String,
    pub(crate) body: w::BodyBinding,
    pub(crate) voice: w::VoiceBinding,
}

impl Hub {
    pub(crate) fn create_companion(&mut self, spec: NewCompanionSpec, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let NewCompanionSpec {
            name,
            persona,
            mind,
            voice_identifier,
        } = spec;
        let default_model = self.model_choices().into_iter().next();
        let voice = voice_identifier
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                self.model.settings().and_then(|settings| {
                    settings
                        .connections
                        .iter()
                        .find(|connection| connection.kind == w::ConnectionKind::Tts)
                        .and_then(|connection| {
                            connection
                                .config
                                .as_option()
                                .and_then(|config| config.config.as_ref())
                        })
                        .and_then(|config| match config {
                            w::connection_config::Config::Tts(tts) => {
                                tts.voice_ids.first().cloned()
                            }
                            _ => None,
                        })
                })
            });
        let assignments = default_model.map(|(conn_id, model_id, _)| w::ModelAssignments {
            companion: Some(w::ModelAssignment {
                connection_id: conn_id.clone(),
                model_id: model_id.clone(),
                ..Default::default()
            })
            .into(),
            task: Some(w::ModelAssignment {
                connection_id: conn_id,
                model_id,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        self.request(
            "コンパニオンの作成",
            async move {
                let response = host
                    .host
                    .create_companion(w::CreateCompanionRequest {
                        name,
                        mind,
                        persona: (!persona.trim().is_empty()).then_some(persona),
                        voice_identifier: voice,
                        assignments: assignments.into(),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |companion| Update::Created(companion.id.clone()),
        );
        cx.notify();
    }

    pub(crate) fn update_companion_profile(
        &mut self,
        patch: CompanionProfilePatch,
        cx: &mut Context<Self>,
    ) {
        let Some(companion) = self
            .model
            .snapshot()
            .and_then(|s| s.companions.iter().find(|c| c.id == patch.companion_id))
            .cloned()
        else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let voice_identifier = patch.voice.voice_id.clone().unwrap_or_default();
        let body_doc = serde_json::to_string(&patch.body)
            .ok()
            .map(|json| w::JsonDocument {
                schema: "ene.body.v1".into(),
                json,
                ..Default::default()
            });
        let voice_doc = serde_json::to_string(&patch.voice)
            .ok()
            .map(|json| w::JsonDocument {
                schema: "ene.voice.v1".into(),
                json,
                ..Default::default()
            });
        self.request(
            "コンパニオンの保存",
            async move {
                let response = host
                    .host
                    .update_companion(w::UpdateCompanionRequest {
                        id: companion.id.clone(),
                        revision: companion.revision,
                        name: (!patch.name.is_empty() && patch.name != companion.name)
                            .then_some(patch.name),
                        persona: Some(patch.persona),
                        mind: (patch.mind != companion.mind).then_some(patch.mind),
                        body: body_doc.into(),
                        voice: voice_doc.into(),
                        voice_identifier: Some(voice_identifier),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            companion_update,
        );
        cx.notify();
    }

    pub(crate) fn load_companion_templates(&mut self, _cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "テンプレート一覧の取得",
            workflows::list_companion_templates(host),
            Update::CompanionTemplates,
        );
    }

    pub(crate) fn export_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let target = companion_id.clone();
        self.request(
            "テンプレートの書き出し",
            workflows::export_companion(host, companion_id),
            move |json| Update::CompanionExported {
                companion_id: target,
                json,
            },
        );
        cx.notify();
    }

    pub(crate) fn import_companion_json(&mut self, raw: String, cx: &mut Context<Self>) {
        let template = match workflows::parse_template_json(&raw) {
            Ok(template) => template,
            Err(error) => {
                self.model.notice(
                    NoticeKind::Failed,
                    "テンプレートの読み込み",
                    error.to_string(),
                );
                cx.notify();
                return;
            }
        };
        self.import_companion_template(template, cx);
    }

    pub(crate) fn import_companion_template(
        &mut self,
        template: w::CompanionTemplate,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        let assignments = self
            .model_choices()
            .into_iter()
            .next()
            .map(|(conn_id, model_id, _)| w::ModelAssignments {
                companion: Some(w::ModelAssignment {
                    connection_id: conn_id.clone(),
                    model_id: model_id.clone(),
                    ..Default::default()
                })
                .into(),
                task: Some(w::ModelAssignment {
                    connection_id: conn_id,
                    model_id,
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            });
        self.request(
            "テンプレートから作成",
            workflows::import_companion(host, template, assignments),
            |companion| Update::Created(companion.id),
        );
        cx.notify();
    }

    pub(crate) fn preview_companion(
        &mut self,
        name: String,
        mind: String,
        persona: String,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        self.model.companions.preview_reply = None;
        let model = self
            .model_choices()
            .into_iter()
            .next()
            .map(|(connection_id, model_id, _)| w::ModelAssignment {
                connection_id,
                model_id,
                ..Default::default()
            });
        let request = w::PreviewCompanionRequest {
            name,
            mind,
            persona: (!persona.trim().is_empty()).then_some(persona),
            model: model.into(),
            user_message: "はじめまして！短く自己紹介してください。".into(),
            ..Default::default()
        };
        self.request(
            "コンパニオンのプレビュー",
            async move { Ok(workflows::preview_companion(host, request).await) },
            |update| update,
        );
        cx.notify();
    }

    pub(crate) fn assign_model(
        &mut self,
        slot: ModelRoleSlot,
        assignment: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(companion) = self.model.companion().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let patch = w::AssignmentPatch {
            change: Some(match assignment {
                Some((connection_id, model_id)) => {
                    w::assignment_patch::Change::Set(Box::new(w::ModelAssignment {
                        connection_id,
                        model_id,
                        ..Default::default()
                    }))
                }
                None => w::assignment_patch::Change::Clear(Box::default()),
            }),
            ..Default::default()
        };
        self.request(
            "モデル割当の変更",
            async move {
                let mut request = w::UpdateCompanionRequest {
                    id: companion.id.clone(),
                    revision: companion.revision,
                    ..Default::default()
                };
                match slot {
                    ModelRoleSlot::Companion => request.companion_assignment = Some(patch).into(),
                    ModelRoleSlot::Task => request.task_assignment = Some(patch).into(),
                    ModelRoleSlot::Perception => request.perception_assignment = Some(patch).into(),
                    ModelRoleSlot::Compaction => request.compaction_assignment = Some(patch).into(),
                    ModelRoleSlot::Consolidation => {
                        request.consolidation_assignment = Some(patch).into()
                    }
                    ModelRoleSlot::Decision => request.decision_assignment = Some(patch).into(),
                    ModelRoleSlot::Embedding => request.embedding_assignment = Some(patch).into(),
                }
                let response = host.host.update_companion(request).await?;
                response.view().to_owned_message().map_err(decode)
            },
            companion_update,
        );
        cx.notify();
    }

    pub(crate) fn delete_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let gone_id = companion_id.clone();
        self.request(
            "コンパニオンの削除",
            async move {
                host.host
                    .delete_companion(w::Id {
                        id: companion_id,
                        ..Default::default()
                    })
                    .await
            },
            move |_| Update::Gone {
                companion_id: gone_id,
            },
        );
        cx.notify();
    }

    pub(crate) fn load_mind(&mut self, _cx: &mut Context<Self>) {
        let Some(companion_id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        self.load_mind_for(companion_id);
    }

    pub(crate) fn load_mind_for(&mut self, companion_id: String) {
        let Some(host) = self.host() else {
            return;
        };
        let target = companion_id.clone();
        self.request(
            "詳細プロフィールの取得",
            async move {
                let response = host
                    .host
                    .get_mind(w::Id {
                        id: companion_id,
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |view| Update::MindLoaded {
                companion_id: target,
                view: Box::new(view),
            },
        );
    }

    pub(crate) fn update_mind_profile(
        &mut self,
        companion_id: String,
        expected_revision: u64,
        profile: w::MindProfile,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        let target = companion_id.clone();
        self.request(
            "詳細プロフィールの保存",
            async move {
                let response = host
                    .host
                    .update_mind_profile(w::UpdateMindProfileRequest {
                        companion_id,
                        expected_revision,
                        profile: Some(profile).into(),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |view| Update::MindLoaded {
                companion_id: target,
                view: Box::new(view),
            },
        );
        cx.notify();
    }
}

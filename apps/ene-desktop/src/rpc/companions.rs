//! Companion CRUD, model assignment, mind profile, template, and preview RPCs.

use super::{RpcDispatcher, companion_update, decode};
use crate::update::Update;
use buffa::view::MessageView;
use ene_client_core::{
    ModelRoleSlot,
    domain::{
        AutoAssignModelPatch, Companion, CompanionProfilePatch, CompanionTemplate, MindProfile,
        ModelAssignment, NewCompanionSpec,
    },
    templates::export_template_json,
    wire::companion::assignment_patch_from,
};
use ene_rpc as w;

fn default_assignments(default_model: Option<(String, String)>) -> Option<w::ModelAssignments> {
    default_model.map(|(conn_id, model_id)| w::ModelAssignments {
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
    })
}

pub(crate) fn create_companion(
    rpc: &RpcDispatcher,
    epoch: u64,
    spec: NewCompanionSpec,
    voice: Option<String>,
    default_model: Option<(String, String)>,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let assignments = default_assignments(default_model);
    rpc.spawn(
        epoch,
        "コンパニオンの作成",
        async move {
            let response = host
                .host
                .create_companion(w::CreateCompanionRequest {
                    name: spec.name,
                    mind: spec.mind,
                    persona: (!spec.persona.trim().is_empty()).then_some(spec.persona),
                    voice_identifier: voice,
                    assignments: assignments.into(),
                    ..Default::default()
                })
                .await?;
            response.view().to_owned_message().map_err(decode)
        },
        |companion| Update::Created(companion.id),
    );
}

pub(crate) fn update_companion_profile(
    rpc: &RpcDispatcher,
    epoch: u64,
    companion: Companion,
    patch: CompanionProfilePatch,
) {
    let Some(host) = rpc.host() else {
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
    rpc.spawn(
        epoch,
        "コンパニオンの保存",
        async move {
            let response = host
                .host
                .update_companion(w::UpdateCompanionRequest {
                    id: companion.id,
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
}

pub(crate) fn assign_model(
    rpc: &RpcDispatcher,
    epoch: u64,
    companion_id: String,
    revision: u64,
    slot: ModelRoleSlot,
    assignment: Option<ModelAssignment>,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let patch = assignment_patch_from(assignment);
    rpc.spawn(
        epoch,
        "モデル割当の変更",
        async move {
            let mut request = w::UpdateCompanionRequest {
                id: companion_id,
                revision,
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
}

pub(crate) fn auto_assign_default_model(
    rpc: &RpcDispatcher,
    epoch: u64,
    patch: AutoAssignModelPatch,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let wire_patch = assignment_patch_from(Some(patch.assignment));
    let request = w::UpdateCompanionRequest {
        id: patch.companion_id,
        revision: patch.revision,
        companion_assignment: patch.assign_companion.then(|| wire_patch.clone()).into(),
        task_assignment: patch.assign_task.then_some(wire_patch).into(),
        ..Default::default()
    };
    rpc.spawn(
        epoch,
        "既定モデルの自動設定",
        async move {
            let response = host.host.update_companion(request).await?;
            response.view().to_owned_message().map_err(decode)
        },
        companion_update,
    );
}

pub(crate) fn sync_companion_voice(
    rpc: &RpcDispatcher,
    epoch: u64,
    companion_id: String,
    revision: u64,
    voice_clean: String,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "コンパニオン話者の同期",
        async move {
            let response = host
                .host
                .update_companion(w::UpdateCompanionRequest {
                    id: companion_id,
                    revision,
                    voice_identifier: Some(voice_clean),
                    ..Default::default()
                })
                .await?;
            response.view().to_owned_message().map_err(decode)
        },
        companion_update,
    );
}

pub(crate) fn delete_companion(rpc: &RpcDispatcher, epoch: u64, companion_id: String) {
    let Some(host) = rpc.host() else {
        return;
    };
    let gone_id = companion_id.clone();
    rpc.spawn(
        epoch,
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
}

pub(crate) fn load_companion_templates(rpc: &RpcDispatcher, epoch: u64) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "テンプレート一覧の取得",
        async move {
            let response = host
                .host
                .list_companion_templates(w::Empty::default())
                .await?;
            Ok(response
                .view()
                .to_owned_message()
                .map_err(decode)?
                .items
                .into_iter()
                .map(Into::into)
                .collect())
        },
        Update::CompanionTemplates,
    );
}

pub(crate) fn export_companion(rpc: &RpcDispatcher, epoch: u64, companion_id: String) {
    let Some(host) = rpc.host() else {
        return;
    };
    let target = companion_id.clone();
    rpc.spawn(
        epoch,
        "テンプレートの書き出し",
        async move {
            let response = host
                .host
                .export_companion(w::Id {
                    id: companion_id,
                    ..Default::default()
                })
                .await?;
            let template = response.view().to_owned_message().map_err(decode)?;
            export_template_json(&template.into())
                .map_err(|error| connectrpc::ConnectError::internal(error.to_string()))
        },
        move |json| Update::CompanionExported {
            companion_id: target,
            json,
        },
    );
}

pub(crate) fn import_companion(
    rpc: &RpcDispatcher,
    epoch: u64,
    template: CompanionTemplate,
    default_model: Option<(String, String)>,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let assignments = default_assignments(default_model);
    rpc.spawn(
        epoch,
        "テンプレートから作成",
        async move {
            let response = host
                .host
                .import_companion(w::ImportCompanionRequest {
                    template: Some(template.into()).into(),
                    assignments: assignments.into(),
                    ..Default::default()
                })
                .await?;
            response.view().to_owned_message().map_err(decode)
        },
        |companion| Update::Created(companion.id),
    );
}

pub(crate) fn preview_companion(
    rpc: &RpcDispatcher,
    epoch: u64,
    name: String,
    mind: String,
    persona: String,
    model: Option<ModelAssignment>,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let request = w::PreviewCompanionRequest {
        name,
        mind,
        persona: (!persona.trim().is_empty()).then_some(persona),
        model: model.map(Into::into).into(),
        user_message: "はじめまして！短く自己紹介してください。".into(),
        ..Default::default()
    };
    rpc.spawn_update(epoch, async move {
        match host.host.preview_companion(request).await {
            Ok(resp) => match resp.view().to_owned_message() {
                Ok(body) => Update::CompanionPreview {
                    reply: Some(body.reply),
                    error: None,
                },
                Err(err) => Update::CompanionPreview {
                    reply: None,
                    error: Some(err.to_string()),
                },
            },
            Err(err) => Update::CompanionPreview {
                reply: None,
                error: Some(err.to_string()),
            },
        }
    });
}

pub(crate) fn load_mind(rpc: &RpcDispatcher, epoch: u64, companion_id: String) {
    let Some(host) = rpc.host() else {
        return;
    };
    let target = companion_id.clone();
    rpc.spawn(
        epoch,
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
            view: Box::new(view.into()),
        },
    );
}

pub(crate) fn update_mind_profile(
    rpc: &RpcDispatcher,
    epoch: u64,
    companion_id: String,
    expected_revision: u64,
    profile: MindProfile,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let target = companion_id.clone();
    rpc.spawn(
        epoch,
        "詳細プロフィールの保存",
        async move {
            let response = host
                .host
                .update_mind_profile(w::UpdateMindProfileRequest {
                    companion_id,
                    expected_revision,
                    profile: Some(profile.into()).into(),
                    ..Default::default()
                })
                .await?;
            response.view().to_owned_message().map_err(decode)
        },
        move |view| Update::MindLoaded {
            companion_id: target,
            view: Box::new(view.into()),
        },
    );
}
